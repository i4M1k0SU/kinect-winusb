use kinect_usb::hex;

use crate::descriptors::parse_config;
use crate::devices::{self, CAMERA_PIDS, GUID_KINECT_CAMERA, GUID_KINECT_DEVICE, VID_MICROSOFT};
use crate::hub::HubPort;

pub fn run(_args: &[String]) -> i32 {
    println!("== Microsoft USB device nodes (VID 045E)");
    let nodes = devices::usb_devices(VID_MICROSOFT, &[]);
    for n in &nodes {
        println!(
            "  {:04X}:{:04X}  {:<40} service={:<10} problem={:<4} port={:?}\n      {}",
            n.vid,
            n.pid,
            n.description.as_deref().unwrap_or("?"),
            n.service.as_deref().unwrap_or("-"),
            n.problem.map_or_default(|p| p.to_string()),
            n.address,
            n.instance_id
        );
    }

    for (name, guid) in [("Kinect camera", &GUID_KINECT_CAMERA), ("Kinect device", &GUID_KINECT_DEVICE)] {
        println!("== interfaces {} {}", name, devices::guid_string(guid));
        for path in devices::interface_paths(guid, None) {
            let id = devices::interface_instance_id(&path).unwrap_or_default();
            println!("  {path}\n      ({id})");
        }
    }

    for n in nodes.iter().filter(|n| CAMERA_PIDS.contains(&n.pid)) {
        println!("== camera {} descriptors (via parent hub)", n.instance_id);
        let port = match HubPort::for_device(n) {
            Ok(p) => p,
            Err(e) => {
                println!("  cannot open hub: {e}");
                continue;
            }
        };
        match port.device_descriptor() {
            Ok(d) if d.len() >= 18 => println!(
                "  device: {}\n    USB {:x}.{:02x}, {:04X}:{:04X} bcdDevice {:04X}, ep0 {} B, {} configuration(s)",
                hex(&d),
                d[3],
                d[2],
                u16::from_le_bytes([d[8], d[9]]),
                u16::from_le_bytes([d[10], d[11]]),
                u16::from_le_bytes([d[12], d[13]]),
                d[7],
                d[17]
            ),
            Ok(d) => println!("  device descriptor too short: {}", hex(&d)),
            Err(e) => println!("  device descriptor failed: {e}"),
        }
        match port.config_descriptor() {
            Ok(c) => {
                println!("  config ({} bytes): {}", c.len(), hex(&c));
                if let Some(cfg) = parse_config(&c) {
                    println!(
                        "  bConfigurationValue={} attributes=0x{:02x} maxPower={}mA",
                        cfg.value, cfg.attributes, cfg.max_power_ma
                    );
                    for itf in &cfg.interfaces {
                        println!(
                            "  interface {} alt {} class {:02x}/{:02x}/{:02x}",
                            itf.number, itf.alternate, itf.class, itf.subclass, itf.protocol
                        );
                        for ep in &itf.endpoints {
                            println!(
                                "    ep 0x{:02x} {:<11} wMaxPacketSize=0x{:04x} ({} B/interval) bInterval={}",
                                ep.address,
                                ep.transfer_type(),
                                ep.max_packet_size,
                                ep.bytes_per_interval(),
                                ep.interval
                            );
                        }
                    }
                }
            }
            Err(e) => println!("  config descriptor failed: {e}"),
        }
    }
    0
}
