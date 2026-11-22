//! The description of one HID top-level collection, free of Win32 so code that only matches
//! collections (event channels, grouping) can be unit tested on any platform.

/// One HID top-level collection.
#[derive(Clone, Debug, Default)]
pub struct HidColl {
    pub path: String,
    pub vid: u16,
    pub pid: u16,
    /// USB interface number (MI_xx in the path), -1 when unknown.
    pub iface: i32,
    pub usage_page: u16,
    pub usage_id: u16,
    pub input_len: u16,
    pub output_len: u16,
    pub feature_len: u16,
    pub n_input_value_caps: u16,
    pub n_feature_value_caps: u16,
}

impl HidColl {
    pub fn page(&self) -> u16 {
        self.usage_page
    }
    pub fn usage(&self) -> u16 {
        self.usage_id
    }
    pub fn in_len(&self) -> u16 {
        self.input_len
    }
    pub fn out_len(&self) -> u16 {
        self.output_len
    }
    pub fn feat_len(&self) -> u16 {
        self.feature_len
    }

    /// Identifies the physical device a HID collection belongs to, so two receivers with the same
    /// product id are polled separately (strips "&colNN" and the last "&NNNN" of the instance part).
    pub fn physical_key(&self) -> String {
        let p = self.path.to_lowercase();
        let parts: Vec<&str> = p.splitn(4, '#').collect();
        if parts.len() < 4 {
            return String::new();
        }
        let mut hw = parts[1].to_string();
        if let Some(col) = hw.find("&col") {
            let end = (col + 6).min(hw.len());
            hw.replace_range(col..end, "");
        }
        let mut inst = parts[2].to_string();
        if let Some(amp) = inst.rfind('&') {
            inst.truncate(amp);
        }
        format!("{hw}#{inst}")
    }
}
