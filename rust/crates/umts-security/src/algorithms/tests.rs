// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

fn hex(value: &str) -> Vec<u8> {
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}
fn key(value: &str) -> Key {
    Key::new(hex(value).try_into().unwrap())
}

#[test]
fn f8_matches_byte_reference_at_every_tail_boundary() {
    let key = Uea1Key::new(&Key::new([0x42; 16]));
    for bits in (1..=513).chain([MAX_F8_BITS - 65, MAX_F8_BITS - 1, MAX_F8_BITS]) {
        for (count, bearer, direction) in [
            (0, 0, Direction::Uplink),
            (u32::MAX, 31, Direction::Downlink),
        ] {
            let mut actual: Vec<u8> = (0..bits.div_ceil(8)).map(|i| i as u8).collect();
            let mut expected = actual.clone();
            let initial = key.initialization.encrypt(
                (u64::from(count) << u32::BITS)
                    | (u64::from(bearer) << BEARER_SHIFT)
                    | (direction_bit(direction) << DIRECTION_SHIFT),
            );
            let mut previous = 0;
            for (block, bytes) in expected.chunks_mut(BLOCK_BYTES).enumerate() {
                previous = key.cipher.encrypt(initial ^ block as u64 ^ previous);
                let stream = previous.to_be_bytes();
                for (offset, byte) in bytes.iter_mut().enumerate() {
                    let consumed = block * BLOCK_BITS + offset * u8::BITS as usize;
                    let used = (bits - consumed).min(u8::BITS as usize);
                    *byte ^= stream[offset] & (u8::MAX << (u8::BITS as usize - used));
                }
            }
            key.apply(count, bearer, direction, &mut actual, bits)
                .unwrap();
            assert_eq!(actual, expected, "bits={bits}");
        }
    }
}

#[test]
fn independent_ts35203_f8_vectors_cover_direction_feedback_and_partial_bytes() {
    for (secret, count, bearer, direction, bits, plain, cipher) in [
        (
            "5ACB1D644C0D51204EA5F1451010D852",
            0xfa556b26,
            3,
            Direction::Downlink,
            120,
            "AD9C441F890B38C457A49D421407E8",
            "9BC92CA803C67B28A11A4BEE5A0C25",
        ),
        (
            "D3C5D592327FB11C4035C6680AF8C6D1",
            0x398a59b4,
            5,
            Direction::Downlink,
            253,
            "981BA6824C1BFB1AB485472029B71D808CE33E2CC3C0B5FC1F3DE8A6DC66B1F0",
            "5BB9431BB1E98BD11B93DB7C3D45136559BB86A295AA204ECBEBF6F7A5101512",
        ),
        (
            "EFA8B2229E720C2A7C36EA55E9605695",
            0xe28bcf7b,
            0x18,
            Direction::Uplink,
            510,
            "10111231E060253A43FD3F57E37607AB2827B599B6B1BBDA37A8ABCC5A8C550D1BFB2F494624FB50367FA36CE3BC68F11CF93B1510376B02130F812A9FA169D8",
            "3DEACC7C15821CAA89EECADE9B5BD3614BD0C8419D710385DDBE5849EF1BAC5AE8B14A5B0A6741521EB4E00BB9ECF3E9F7CCB9CAE74152D7F4E2A034B6EA00EC",
        ),
        (
            "2BD6459F82C5B300952C49104881FF48",
            0x72a4f20f,
            0xc,
            Direction::Downlink,
            798,
            "7EC61272743BF1614726446A6C38CED166F6CA76EB5430044286346CEF130F92922B03450D3A9975E5BD2EA0EB55AD8E1B199E3EC4316020E9A1B285E762795359B7BDFD39BEF4B2484583D5AFE082AEE638BF5FD5A606193901A08F4AB41AAB9B134880",
            "D1E2DE70EEF86C6964FB542BC2D460AABFAA10A4A093262B7D199E706FC2D4891553296910F3A973012682E41C4E2B02BE2017B7253BBF9309DE5819CB42E81956F4C99BC9765CAF53B1D0BB8279826ADBBC5522E915C120A618A5A7F5E897089339650F",
        ),
    ] {
        let algorithm = Uea1Key::new(&key(secret));
        let original = hex(plain);
        let mut actual = original.clone();
        algorithm
            .apply(count, bearer, direction, &mut actual, bits)
            .unwrap();
        let expected = hex(cipher);
        for bit in 0..bits {
            let mask = 1 << (7 - bit % 8);
            assert_eq!(
                actual[bit / 8] & mask,
                expected[bit / 8] & mask,
                "bit {bit} of {bits}"
            );
        }
        if bits % 8 != 0 {
            let mask = u8::MAX >> (bits % 8);
            assert_eq!(
                actual.last().unwrap() & mask,
                original.last().unwrap() & mask
            );
        }
        algorithm
            .apply(count, bearer, direction, &mut actual, bits)
            .unwrap();
        assert_eq!(actual, original);
    }
}

#[test]
fn all_five_independent_ts35203_f9_vectors_cover_padding_boundaries() {
    for (secret, count, fresh, direction, bits, message, expected) in [
        (
            "2BD6459F82C5B300952C49104881FF48",
            0x38a6f056,
            0x05d2ec49,
            Direction::Uplink,
            189,
            "6B227737296F393C8079353EDC87E2E805D2EC49A4F2D8E0",
            0xf63bd72c_u32,
        ),
        (
            "D42F682428201CAFCD9F97945E6DE7B7",
            0x3edc87e2,
            0xa4f2d8e2,
            Direction::Downlink,
            254,
            "B5924384328A4AE00B737109F8B6C8DD2B4DB63DD533981CEB19AAD52A5B2BC0",
            0xa9daf1ff,
        ),
        (
            "FDB9CFDF28936CC483A31869D81B8FAB",
            0x36af6144,
            0x9838f03a,
            Direction::Downlink,
            319,
            "5932BC0ACE2B0ABA33D8AC188AC54F346FAD10BF9DEE2920B43BD0C53A915CB7DF6CAA72053ABFF2",
            0x1537d316,
        ),
        (
            "C736C6AAB22BFFF91E2698D2E22AD57E",
            0x14793e41,
            0x0397e8fd,
            Direction::Downlink,
            384,
            "D0A7D463DF9FB2B278833FA02E235AA172BD970C1473E12907FB648B6599AAA0B24A038665422B20A499276A50427009",
            0xdd7dfadd,
        ),
        (
            "F4EBEC69E73EAF2EB2CF6AF4B3120FFD",
            0x296f393c,
            0x6b227737,
            Direction::Downlink,
            1000,
            "10BFFF839E0C71658DBB2D1707E145724F41C16F48BF403C3B18E38FD5D1663B6F6D900193E3CEA8BB4F1B4F5BE822032232A78D7D75238D5E6DAECD3B4322CF59BC7EA84AB18811B5BFB7BC553F4FE44478CE287A14879990D18D12CA79D2C855149021CD5CE8CA0371CA04FCCE143E3D7CFEE94585B5885CAC46068B",
            0xc383839d,
        ),
    ] {
        let algorithm = Uia1Key::new(&key(secret));
        let message = hex(message);
        assert_eq!(
            algorithm
                .authenticate(count, fresh, direction, &message, bits)
                .unwrap(),
            expected.to_be_bytes()
        );
    }
}

#[test]
fn invalid_inputs_preserve_f8_output_and_unused_bits_do_not_enter_f9() {
    let key = Key::new([0; 16]);
    let f8 = Uea1Key::new(&key);
    for (bearer, length, expected) in [
        (32, 8, Error::Bearer),
        (0, 0, Error::Length),
        (0, 9, Error::Length),
        (0, MAX_F8_BITS + 1, Error::Length),
    ] {
        let mut bytes = [0xa5];
        assert_eq!(
            f8.apply(0, bearer, Direction::Uplink, &mut bytes, length),
            Err(expected)
        );
        assert_eq!(bytes, [0xa5]);
    }
    let f9 = Uia1Key::new(&key);
    assert_eq!(
        f9.authenticate(0, 0, Direction::Uplink, &[0x80], 1),
        f9.authenticate(0, 0, Direction::Uplink, &[0xff], 1)
    );
    assert_eq!(
        f9.authenticate(0, 0, Direction::Uplink, &[], 1),
        Err(Error::Length)
    );
}

#[test]
fn f8_prefix_is_independent_of_requested_length_and_validates_the_maximum() {
    let algorithm = Uea1Key::new(&Key::new([0x5a; 16]));
    let mut full = vec![0; MAX_F8_BITS / 8];
    algorithm
        .apply(9, 31, Direction::Downlink, &mut full, MAX_F8_BITS)
        .unwrap();
    for bits in [1, 7, 8, 63, 64, 65, 127, 128, 129, MAX_F8_BITS - 1] {
        let mut bytes = vec![0; bits.div_ceil(8)];
        algorithm
            .apply(9, 31, Direction::Downlink, &mut bytes, bits)
            .unwrap();
        for bit in 0..bits {
            let mask = 1 << (7 - bit % 8);
            assert_eq!(bytes[bit / 8] & mask, full[bit / 8] & mask);
        }
    }
}
