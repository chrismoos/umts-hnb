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

// TS 35.217 implementors' f8/f9 test sets.
#[test]
fn snow3g_f8_testset_1() {
    let key = key("2bd6459f82c5b300952c49104881ff48");
    let input = hex(
        "7ec61272743bf1614726446a6c38ced166f6ca76eb5430044286346cef130f92922b03450d3a9975e5bd2ea0eb55ad8e1b199e3ec4316020e9a1b285e762795359b7bdfd39bef4b2484583d5afe082aee638bf5fd5a606193901a08f4ab41aab9b134880",
    );
    let expected = hex(
        "8ceba62943dced3a0990b06ea1b0a2c4fb3cedc71b369f42ba64c1eb6665e72aa1c9bb0deaa20fe86058b8baee2c2e7f0becce48b52932a53c9d5f931a3a7c532259af4325e2a65e3084ad5f6a513b7bddc1b65f0aa0d97a053db55a88c4c4f9605e4140",
    );
    let cipher = Uea2Key::new(&key);
    let mut actual = input.clone();
    cipher
        .apply(1923412495, 12, Direction::Downlink, &mut actual, 798)
        .unwrap();
    let used = 798 % 8;
    if used != 0 {
        *actual.last_mut().unwrap() &= u8::MAX << (8 - used);
    }
    assert_eq!(actual, expected);
    cipher
        .apply(1923412495, 12, Direction::Downlink, &mut actual, 798)
        .unwrap();
    let mut original = input;
    if used != 0 {
        *original.last_mut().unwrap() &= u8::MAX << (8 - used);
    }
    assert_eq!(actual, original);
}
#[test]
fn snow3g_f8_testset_2() {
    let key = key("efa8b2229e720c2a7c36ea55e9605695");
    let input = hex(
        "10111231e060253a43fd3f57e37607ab2827b599b6b1bbda37a8abcc5a8c550d1bfb2f494624fb50367fa36ce3bc68f11cf93b1510376b02130f812a9fa169d8",
    );
    let expected = hex(
        "e0da15ca8e2554f5e56c9468dc6c7c129c568aa5032317e04e0729646cabefa689864c410f24f919e61e3dfdfad77e560db0a9cd36c34ae4181490b29f5fa2fc",
    );
    let cipher = Uea2Key::new(&key);
    let mut actual = input.clone();
    cipher
        .apply(3800813435, 24, Direction::Uplink, &mut actual, 510)
        .unwrap();
    let used = 510 % 8;
    if used != 0 {
        *actual.last_mut().unwrap() &= u8::MAX << (8 - used);
    }
    assert_eq!(actual, expected);
    cipher
        .apply(3800813435, 24, Direction::Uplink, &mut actual, 510)
        .unwrap();
    let mut original = input;
    if used != 0 {
        *original.last_mut().unwrap() &= u8::MAX << (8 - used);
    }
    assert_eq!(actual, original);
}
#[test]
fn snow3g_f8_testset_3() {
    let key = key("5acb1d644c0d51204ea5f1451010d852");
    let input = hex("ad9c441f890b38c457a49d421407e8");
    let expected = hex("ba0f31300334c56b52a7497cbac046");
    let cipher = Uea2Key::new(&key);
    let mut actual = input.clone();
    cipher
        .apply(4199901990, 3, Direction::Downlink, &mut actual, 120)
        .unwrap();
    let used = 120 % 8;
    if used != 0 {
        *actual.last_mut().unwrap() &= u8::MAX << (8 - used);
    }
    assert_eq!(actual, expected);
    cipher
        .apply(4199901990, 3, Direction::Downlink, &mut actual, 120)
        .unwrap();
    let mut original = input;
    if used != 0 {
        *original.last_mut().unwrap() &= u8::MAX << (8 - used);
    }
    assert_eq!(actual, original);
}
#[test]
fn snow3g_f8_testset_4() {
    let key = key("d3c5d592327fb11c4035c6680af8c6d1");
    let input = hex("981ba6824c1bfb1ab485472029b71d808ce33e2cc3c0b5fc1f3de8a6dc66b1f0");
    let expected = hex("989b719cdc33ceb7cf276a52827cef94a56c40c0ab9d81f7a2a9bac60e11c4b0");
    let cipher = Uea2Key::new(&key);
    let mut actual = input.clone();
    cipher
        .apply(965368244, 5, Direction::Downlink, &mut actual, 253)
        .unwrap();
    let used = 253 % 8;
    if used != 0 {
        *actual.last_mut().unwrap() &= u8::MAX << (8 - used);
    }
    assert_eq!(actual, expected);
    cipher
        .apply(965368244, 5, Direction::Downlink, &mut actual, 253)
        .unwrap();
    let mut original = input;
    if used != 0 {
        *original.last_mut().unwrap() &= u8::MAX << (8 - used);
    }
    assert_eq!(actual, original);
}
#[test]
fn snow3g_f8_testset_5() {
    let key = key("6090eae04c83706eecbf652be8e36566");
    let input = hex(
        "40981ba6824c1bfb4286b299783daf442c099f7ab0f58d5c8e46b104f08f01b41ab485472029b71d36bd1a3d90dc3a41b46d51672ac4c9663a2be063da4bc8d2808ce33e2cccbfc634e1b259060876a0fbb5a437ebcc8d31c19e4454318745e3987645987a986f2cb0",
    );
    let expected = hex(
        "5892bba88bbbcaaeae769aa06b683d3a17cc04a369881697435e44fed5ff9af57b9e890d4d5c64709885d48ae40690ec043baae9705796e4a9ff5a4b8d8b36d7f3fe57cc6cfd6cd005cd3852a85e94ce6bcd90d0d07839ce09733544ca8e350843248550922ac12818",
    );
    let cipher = Uea2Key::new(&key);
    let mut actual = input.clone();
    cipher
        .apply(1923412495, 9, Direction::Uplink, &mut actual, 837)
        .unwrap();
    let used = 837 % 8;
    if used != 0 {
        *actual.last_mut().unwrap() &= u8::MAX << (8 - used);
    }
    assert_eq!(actual, expected);
    cipher
        .apply(1923412495, 9, Direction::Uplink, &mut actual, 837)
        .unwrap();
    let mut original = input;
    if used != 0 {
        *original.last_mut().unwrap() &= u8::MAX << (8 - used);
    }
    assert_eq!(actual, original);
}
#[test]
fn snow3g_f9_testset_1() {
    let key = key("2bd6459f82c5b300952c49104881ff48");
    let input = hex("6b227737296f393c8079353edc87e2e805d2ec49a4f2d8e0");
    let expected = hex("2bce1820");
    assert_eq!(
        Uia2Key::new(&key)
            .authenticate(950464598, 97709129, Direction::Uplink, &input, 189)
            .unwrap()
            .as_slice(),
        expected
    );
}
#[test]
fn snow3g_f9_testset_2() {
    let key = key("d42f682428201cafcd9f97945e6de7b7");
    let input = hex("b5924384328a4ae00b737109f8b6c8dd2b4db63dd533981ceb19aad52a5b2bc0");
    let expected = hex("fc7b18bd");
    assert_eq!(
        Uia2Key::new(&key)
            .authenticate(1054640098, 2767378658, Direction::Downlink, &input, 254)
            .unwrap()
            .as_slice(),
        expected
    );
}
#[test]
fn snow3g_f9_testset_3() {
    let key = key("fdb9cfdf28936cc483a31869d81b8fab");
    let input =
        hex("5932bc0ace2b0aba33d8ac188ac54f346fad10bf9dee2920b43bd0c53a915cb7df6caa72053abff2");
    let expected = hex("02f1faaf");
    assert_eq!(
        Uia2Key::new(&key)
            .authenticate(917463364, 2553868346, Direction::Downlink, &input, 319)
            .unwrap()
            .as_slice(),
        expected
    );
}
#[test]
fn snow3g_f9_testset_4() {
    let key = key("c736c6aab22bfff91e2698d2e22ad57e");
    let input = hex(
        "d0a7d463df9fb2b278833fa02e235aa172bd970c1473e12907fb648b6599aaa0b24a038665422b20a499276a50427009",
    );
    let expected = hex("38b554c0");
    assert_eq!(
        Uia2Key::new(&key)
            .authenticate(343490113, 60287229, Direction::Downlink, &input, 384)
            .unwrap()
            .as_slice(),
        expected
    );
}
#[test]
fn snow3g_f9_testset_5() {
    let key = key("f4ebec69e73eaf2eb2cf6af4b3120ffd");
    let input = hex(
        "10bfff839e0c71658dbb2d1707e145724f41c16f48bf403c3b18e38fd5d1663b6f6d900193e3cea8bb4f1b4f5be822032232a78d7d75238d5e6daecd3b4322cf59bc7ea84ab18811b5bfb7bc553f4fe44478ce287a14879990d18d12ca79d2c855149021cd5ce8ca0371ca04fcce143e3d7cfee94585b5885cac46068b",
    );
    let expected = hex("061745ae");
    assert_eq!(
        Uia2Key::new(&key)
            .authenticate(695155004, 1797420855, Direction::Downlink, &input, 1000)
            .unwrap()
            .as_slice(),
        expected
    );
}
#[test]
fn snow3g_f9_testset_6() {
    let key = key("b3120ffdb2cf6af4e73eaf2ef4ebec69");
    let input = hex(
        "00000000000000000101010101010101e0958045f3a0bba4e3968346f0a3b8a7c02a018ae640765226b987c913e6cbf083570016cf83efbc61c082513e21561a427c009d28c298eface78ed6d56c2d4505ad032e9c04dc60e73a81696da665c6c48603a57b45ab33221585e68ee3169187fb0239528632dd656c807ea3248b7b46d002b2b5c7458eb85b9ce95879e0340859055e3b0abbc3eace8719caa80265c97205d5dc4bcc902fe1839629ed71328a0f0449f588557e6898860e042aecd84b2404c212c9222da5bf8a89ef6797870cf50771a60f66a2ee62853657addf04cdde07fa414e11f12b4d81b9b4e8ac538ea30666688d881f6c348421992f31b94f8806ed8fccff4c9123b89642527ad613b109bf75167485f1268bf884b4cd23d29a0934925703d634098f7767f1be7491e708a8bb949a3873708aef4a36239e50cc08235cd5ed6bbe578668a17b58c1171d0b90e813a9e4f58a89d719b11042d6360b1b0f52deb730a58d58faf46315954b0a872691475977dc88c0d733feff54600a0cc1d0300aaaeb94572c6e95b01ae90de04f1dce47f87e8fa7bebf77e1dbc20d6ba85cb9143d518b285dfa04b698bf0cf7819f20fa7a288eb0703d995c59940c7c66de57a9b70f82379b70e2031e450fcfd2181326fcd28d8823baaa80df6e0f443559647539fd8907c0ffd9d79c130ed81c9afd9b7e848c9fed38443d5d380e53fbdb8ac8c3d3f06876054f122461107de92fea09c6f6923a188d53afe54a10f60e6e9d5a03d996b5fbc820f8a637116a27ad04b444a0932dd60fbd12671c11e1c0ec73e789879faa3d42c64d20cd1252742a3768c25a901585888ecee1e612d9936b403b0775949a66cdfd99a29b1345baa8d9d5400c91024b0a607363b013ce5de9ae869d3b8d95b0570b3c2d391422d32450cbcfae96652286e96dec1214a9346527980a8192eac1c39a3aaf6f15351da6be764df89772ec0407d06e4415befae7c92580df9bf507497c8f2995160d4e218daacb02944abf83340ce8be1686a960faf90e2d90c55cc6475babc3171a80a363174954955d7101dab16ae8179167e21444b443a9eaaa7c91de36d118c39d389f8dd4469a846c9a262bf7fa18487a79e8de11699e0b8fdf557cb48719d453ba713056109b93a218c89675ac195fb4fb06639b3797144955b3c9327d1aec003d42ecd0ea98abf19ffb4af3561a67e77c35bf15c59c2412da881db02b1bfbcebfac5152bc99bc3f1d15f771001b7029fedb028f8b852bc4407eb83f891c9ca733254fdd1e9edb56919ce9fea21c174072521c18319a54b5d4efbebddf1d8b69b1cbf25f489fcc981372547cf41d008ef0bca1926f934b735e090b3b251eb33a36f82ed9b29cf4cb944188fa0e1e38dd778f7d1c9d987b28d132dfb9731fa4f4b416935be49de30516af3578581f2f13f561c0663361941eab249a4bc123f8d15cd711a956a1bf20fe6eb78aea2373361da0426c79a530c3bb1de0c99722ef1fde39ac2b00a0a8ee7c800a08bc2264f89f4effe627ac2f0531fb554f6d21d74c590a70adfaa390bdfbb3d68e46215cab187d2368d5a71f5ebec081cd3b20c082dbe4cd2faca28773795d6b0c10204b659a939ef29bbe1088243624429927a7eb576dd3a00ea5e01af5d47583b2272c0c161a806521a16ff9b0a722c0cf26b025d5836e2258a4f7d4773ac801e4263bc294f43def7fa8703f3a4197463525887652b0b2a4a2a7cf87f00914871e25039113c7e1618da34064b57a43c463249fb8d05e0f26f4a6d84972e7a9054824145f91295cdbe39a6f920facc659712b46a54ba295bbe6a90154e91b33985a2bcd420ad5c67ec9ad8eb7ac6864db272a516bc94c2839b0a8169a6bf58e1a0c2ada8c883b7bf497a49171268ed15ddd2969384e7ff4bf4aab2ec9ecc6529cf629e2df0f08a77a65afa12aa9b505df8b287ef6cc91493d1caa39076e28ef1ea028f5118de61ae02bb6aefc3343a050292f199f401857b2bead5e6ee2a1f191022f9278016f047791a9d18da7d2a6d27f2e0e51c2f6ea30e8ac49a0604f4c13542e85b68381b9fdcfa0ce4b2d341354852d360245c536b612af71f3e77c9095ae2dbde504b265733dabfe10a20fc7d6d32c21ccc72b8b3444ae663d65922d17f82caa2b865cd88913d291a65899026ea1328439723c198c36b0c3c8d085bfaf8a320fde334b4a4919b44c2b95f6e8ecf73393f7f0d2a40e60b1d406526b022ddc331810b1a5f7c347bd53ed1f105d6a0d30aba477e178889ab2ec55d558deab2630204336962b4db5b663b6902b89e85b31bc6af50fc50accb3fb9b57b663297031378db47896d7fbaf6c600add2c67f936db037986db856eb49cf2db3f7da6d23650e438f1884041b013119e4c2ae5af37cccdfb68660738b58b3c59d1c0248437472aba1f35ca1fb90cd714aa9f635534f49e7c5bba81c2b6b36fdee21ca27e347f793d2ce944edb23c8c9b914be10335e350feb5070394b7a4a15c0ca120283568b7bfc254fe838b137a2147ce7c113a3a4d65499d9e86b87dbcc7f03bbd3a3ab1aa243ece5ba9bcf25f82836cfe473b2d83e7a7201cd0b96a72451e863f6c3ba664a6d073d1f7b5ed990865d978bd3815d06094fc9a2aba5221c22d5ab996389e3721e3af5f05beddc2875e0dfaeb39021ee27a41187cbb45ef40c3e73bc03989f9a30d12c54ba7d2141da8a875493e65776ef35f97debc2286cc4af9b4623eee902f840c52f1b8ad658939aef71f3f72b9ec1de21588bd35484ea44436343ff95ead6ab1d8afb1b2a303df1b71e53c4aea6b2e3e9372be0d1bc99798b0ce3cc10d2a596d565dba82f88ce4cff3b33d5d24e9c0831124bf1ad54b792532983dd6c3a8b7d0",
    );
    let expected = hex("179f2fa6");
    assert_eq!(
        Uia2Key::new(&key)
            .authenticate(695155004, 1797420855, Direction::Downlink, &input, 16448)
            .unwrap()
            .as_slice(),
        expected
    );
}
#[test]
fn invalid_cipher_inputs_do_not_modify_plaintext() {
    let cipher = Uea2Key::new(&Key::new([0; 16]));
    let mut bytes = [0xff; 2];
    assert_eq!(
        cipher.apply(0, 32, Direction::Uplink, &mut bytes, 16),
        Err(Error::Bearer)
    );
    assert_eq!(
        cipher.apply(0, 0, Direction::Uplink, &mut bytes, 8),
        Err(Error::Length)
    );
    assert_eq!(bytes, [0xff; 2]);
}
#[test]
fn unused_bits_are_preserved_and_not_authenticated() {
    let secret = Key::new([0x52; 16]);
    let cipher = Uea2Key::new(&secret);
    let integrity = Uia2Key::new(&secret);
    let mut bytes = [0xff];
    cipher
        .apply(3, 1, Direction::Uplink, &mut bytes, 1)
        .unwrap();
    assert_eq!(bytes[0] & 0x7f, 0x7f);
    assert_eq!(
        integrity.authenticate(3, 4, Direction::Uplink, &[0x80], 1),
        integrity.authenticate(3, 4, Direction::Uplink, &[0xff], 1)
    );
}
