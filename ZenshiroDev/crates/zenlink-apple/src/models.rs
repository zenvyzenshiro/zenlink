//! Device model names, the same tables as the Python backend's apple/models.py:
//! the exact model from lockdown's ProductType, else the series from the chip ID (CPID)
//! at the start of a modern UDID.

const PRODUCT_TYPES: &[(&str, &str)] = &[
    ("iPhone14,4", "iPhone 13 mini"),
    ("iPhone14,5", "iPhone 13"),
    ("iPhone14,2", "iPhone 13 Pro"),
    ("iPhone14,3", "iPhone 13 Pro Max"),
    ("iPhone14,6", "iPhone SE (3rd gen)"),
    ("iPhone14,7", "iPhone 14"),
    ("iPhone14,8", "iPhone 14 Plus"),
    ("iPhone15,2", "iPhone 14 Pro"),
    ("iPhone15,3", "iPhone 14 Pro Max"),
    ("iPhone15,4", "iPhone 15"),
    ("iPhone15,5", "iPhone 15 Plus"),
    ("iPhone16,1", "iPhone 15 Pro"),
    ("iPhone16,2", "iPhone 15 Pro Max"),
    ("iPhone17,3", "iPhone 16"),
    ("iPhone17,4", "iPhone 16 Plus"),
    ("iPhone17,1", "iPhone 16 Pro"),
    ("iPhone17,2", "iPhone 16 Pro Max"),
    ("iPhone17,5", "iPhone 16e"),
    ("iPhone18,3", "iPhone 17"),
    ("iPhone18,4", "iPhone Air"),
    ("iPhone18,1", "iPhone 17 Pro"),
    ("iPhone18,2", "iPhone 17 Pro Max"),
    ("iPad13,18", "iPad (10th gen)"),
    ("iPad13,19", "iPad (10th gen)"),
    ("iPad15,7", "iPad (11th gen)"),
    ("iPad15,8", "iPad (11th gen)"),
    ("iPad14,1", "iPad mini (6th gen)"),
    ("iPad14,2", "iPad mini (6th gen)"),
    ("iPad16,1", "iPad mini (A17 Pro)"),
    ("iPad16,2", "iPad mini (A17 Pro)"),
    ("iPad14,8", "iPad Air 11-inch (M2)"),
    ("iPad14,9", "iPad Air 11-inch (M2)"),
    ("iPad14,10", "iPad Air 13-inch (M2)"),
    ("iPad14,11", "iPad Air 13-inch (M2)"),
    ("iPad15,3", "iPad Air 11-inch (M3)"),
    ("iPad15,4", "iPad Air 11-inch (M3)"),
    ("iPad15,5", "iPad Air 13-inch (M3)"),
    ("iPad15,6", "iPad Air 13-inch (M3)"),
    ("iPad16,3", "iPad Pro 11-inch (M4)"),
    ("iPad16,4", "iPad Pro 11-inch (M4)"),
    ("iPad16,5", "iPad Pro 13-inch (M4)"),
    ("iPad16,6", "iPad Pro 13-inch (M4)"),
];

const IPHONE: &[(&str, &str)] = &[
    ("8020", "iPhone XS / XR"),
    ("8030", "iPhone 11 series"),
    ("8101", "iPhone 12 series"),
    ("8110", "iPhone 13 / 14"),
    ("8120", "iPhone 15 / 14 Pro"),
    ("8130", "iPhone 15 Pro"),
    ("8140", "iPhone 16 series"),
    ("8150", "iPhone 17 series"),
];

const IPAD: &[(&str, &str)] = &[
    ("8020", "iPad (8th gen)"),
    ("8030", "iPad (9th gen)"),
    ("8101", "iPad (10th gen)"),
    ("8103", "iPad Pro / Air (M1)"),
    ("8110", "iPad mini (6th gen)"),
    ("8112", "iPad Pro / Air (M2)"),
    ("8120", "iPad (11th gen)"),
    ("8122", "iPad Air (M3)"),
    ("8130", "iPad mini (A17 Pro)"),
    ("8132", "iPad Pro (M4)"),
];

fn find(table: &[(&str, &'static str)], key: &str) -> Option<&'static str> {
    table.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// Marketing name of a ProductType ("iPhone18,3" -> "iPhone 17").
pub fn product_name(product_type: &str) -> Option<&'static str> {
    find(PRODUCT_TYPES, product_type)
}

/// `family` is "iPhone", "iPad" or anything else (then the family itself is returned).
pub fn series_name(udid: &str, family: &str) -> String {
    let id = udid.replace('-', "");
    let cpid = if id.starts_with("0000") {
        id.get(4..8).unwrap_or_default().to_uppercase()
    } else {
        String::new()
    };
    let table = match family {
        "iPad" => IPAD,
        "iPhone" => IPHONE,
        _ => &[],
    };
    find(table, &cpid).map_or_else(|| family.to_string(), str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(product_name("iPhone18,3"), Some("iPhone 17"));
        assert_eq!(product_name("iPad15,7"), Some("iPad (11th gen)"));
        assert_eq!(
            series_name("00008150-000202D02299401C", "iPhone"),
            "iPhone 17 series"
        );
        assert_eq!(
            series_name("00008120-00164C600CB80032", "iPad"),
            "iPad (11th gen)"
        );
        assert_eq!(series_name("abc", "iOS device"), "iOS device");
    }
}
