// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

fn item(id: u8) -> SetupOrModify {
    SetupOrModify {
        first: First::modification(id),
        second: Second::default(),
    }
}
fn extension(id: u16, value: u8) -> Ie {
    Ie::new(id, Criticality::Ignore, &value).unwrap()
}

#[test]
fn absent_modification_fields_inherit_without_changing_the_original() {
    let mut previous = item(5);
    previous.first.nas_synchronization = Some(fixed(&[0xa0]).unwrap());
    previous.first.user_plane = Some(UserPlane::new(Mode::Transparent, 3).unwrap());
    previous.first.transport = Some(Transport::new(
        Address::from_ip(Domain::Ps, "192.0.2.1".parse().unwrap()).unwrap(),
        Association::gtp(0),
    ));
    previous.second.pdp_types = Some(PdpTypes::new(vec![PdpType::Ipv4]).unwrap());
    previous.second.reporting = Some(Reporting::Report);
    previous.second.uplink_gtp_sequence = Some(u16::MAX);
    previous.second.downlink_gtp_sequence = Some(42);
    previous.second.downlink_npdu_sequence = Some(12);
    previous.second.uplink_npdu_sequence = Some(13);
    assert_eq!(item(5).merged(&previous).unwrap(), previous);
    assert!(item(6).merged(&previous).is_err());
}

#[test]
fn explicit_zero_sequence_overrides_but_other_optional_values_remain() {
    let mut previous = item(5);
    previous.second.uplink_gtp_sequence = Some(42);
    previous.second.downlink_gtp_sequence = Some(43);
    previous.second.reporting = Some(Reporting::Report);
    let mut current = item(5);
    current.second.uplink_gtp_sequence = Some(0);
    current.second.reporting = Some(Reporting::DoNotReport);
    let merged = current.merged(&previous).unwrap();
    assert_eq!(merged.second.uplink_gtp_sequence, Some(0));
    assert_eq!(merged.second.downlink_gtp_sequence, Some(43));
    assert_eq!(merged.second.reporting, Some(Reporting::DoNotReport));
    assert_eq!(previous.second.uplink_gtp_sequence, Some(42));
}

#[test]
fn extension_modification_overrides_only_its_ie_and_rejects_duplicates() {
    let mut previous = item(5);
    previous.first.extensions = Some(ExtensionList(vec![
        extension(50_000, 1),
        extension(50_001, 2),
    ]));
    previous.second.extensions = Some(ExtensionList(vec![extension(50_002, 3)]));
    let mut current = item(5);
    current.first.extensions = Some(ExtensionList(vec![
        extension(50_001, 4),
        extension(50_003, 5),
    ]));
    let merged = current.merged(&previous).unwrap();
    let ies = &merged.first.extensions.as_ref().unwrap().0;
    assert_eq!(
        ies.iter().map(|ie| ie.id).collect::<Vec<_>>(),
        [50_000, 50_001, 50_003]
    );
    assert_eq!(
        ies.iter()
            .map(|ie| ie.decode::<u8>().unwrap())
            .collect::<Vec<_>>(),
        [1, 4, 5]
    );
    assert_eq!(merged.second.extensions, previous.second.extensions);
    current
        .first
        .extensions
        .as_mut()
        .unwrap()
        .0
        .push(extension(50_001, 6));
    assert_eq!(current.merged(&previous), Err(Error::DuplicateIe(50_001)));
}
