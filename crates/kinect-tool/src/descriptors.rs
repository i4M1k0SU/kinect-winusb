//! Minimal USB descriptor parsing.

#[derive(Debug, Clone)]
pub struct Endpoint {
    pub address: u8,
    pub attributes: u8,
    pub max_packet_size: u16,
    pub interval: u8,
}

impl Endpoint {
    pub fn transfer_type(&self) -> &'static str {
        match self.attributes & 3 {
            0 => "control",
            1 => "isochronous",
            2 => "bulk",
            _ => "interrupt",
        }
    }
    /// Bytes per (micro)frame for high-speed high-bandwidth endpoints.
    pub fn bytes_per_interval(&self) -> u32 {
        let base = (self.max_packet_size & 0x7FF) as u32;
        let mult = ((self.max_packet_size >> 11) & 3) as u32 + 1;
        base * mult
    }
}

#[derive(Debug, Clone)]
pub struct InterfaceAlt {
    pub number: u8,
    pub alternate: u8,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub endpoints: Vec<Endpoint>,
}

#[derive(Debug, Clone)]
pub struct ConfigDescriptor {
    pub value: u8,
    pub attributes: u8,
    pub max_power_ma: u16,
    pub interfaces: Vec<InterfaceAlt>,
}

pub fn parse_config(buf: &[u8]) -> Option<ConfigDescriptor> {
    if buf.len() < 9 || buf[1] != 2 {
        return None;
    }
    let mut cfg =
        ConfigDescriptor { value: buf[5], attributes: buf[7], max_power_ma: buf[8] as u16 * 2, interfaces: Vec::new() };
    let mut i = buf[0] as usize;
    while i + 2 <= buf.len() {
        let len = buf[i] as usize;
        if len < 2 || i + len > buf.len() {
            break;
        }
        let d = &buf[i..i + len];
        match d[1] {
            4 if len >= 9 => cfg.interfaces.push(InterfaceAlt {
                number: d[2],
                alternate: d[3],
                class: d[5],
                subclass: d[6],
                protocol: d[7],
                endpoints: Vec::new(),
            }),
            5 if len >= 7 => {
                if let Some(itf) = cfg.interfaces.last_mut() {
                    itf.endpoints.push(Endpoint {
                        address: d[2],
                        attributes: d[3],
                        max_packet_size: u16::from_le_bytes([d[4], d[5]]),
                        interval: d[6],
                    });
                }
            }
            _ => {}
        }
        i += len;
    }
    Some(cfg)
}
