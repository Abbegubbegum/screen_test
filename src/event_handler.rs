// This is copied from keyboard_test, my other rust project

use crossbeam_channel::Sender;
use evdev::{Device, EventSummary, KeyCode};
use std::{fs, io, vec};
use std::{thread, time::Duration};

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub name: String,
}

#[derive(Debug)]
pub enum AppEvent {
    Key { code: KeyCode, info: DeviceInfo },
}

pub fn spawn_device_listeners(tx: &Sender<AppEvent>) -> io::Result<()> {
    let devices = get_devices();

    if devices.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "No input devices found",
        ));
    }

    /*
    println!("Found {} input devices:", devices.len());
    for (_, info) in &devices {
        println!("{}", info.name);
    }
     */

    for (mut dev, info) in devices {
        let tx_clone = tx.clone();

        thread::spawn(move || {
            thread::sleep(Duration::from_millis(100)); // Allow some stagger time

            loop {
                match dev.fetch_events() {
                    Ok(events) => {
                        for event in events {
                            match event.destructure() {
                                EventSummary::Key(_, code, 1) => {
                                    _ = tx_clone.send(AppEvent::Key {
                                        code,
                                        info: info.clone(),
                                    });
                                }
                                _ => {
                                    // Handle other events if needed
                                    // For now, we only care about key events
                                    continue;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("Error fetching events from device {}: {}", info.name, e);
                        break; // Exit the loop on error
                    }
                }
            }
        });
    }

    Ok(())
}

fn get_devices() -> Vec<(Device, DeviceInfo)> {
    let mut devices: Vec<(Device, DeviceInfo)> = vec![];

    let dir = fs::read_dir("/dev/input").expect("Failed to read /dev/input directory");

    for entry in dir.filter_map(Result::ok) {
        if !entry.file_name().to_string_lossy().starts_with("event") {
            continue;
        }

        match Device::open(entry.path()) {
            Ok(device) => {
                let name = device.name().unwrap_or("Unknown").to_string();

                devices.push((
                    device,
                    DeviceInfo {
                        name,
                    },
                ))

                /*
                // A way to check if the device is a keyboard is to check if supported keys include KEY_A
                if device
                    .supported_keys()
                    .map_or(false, |keys| keys.contains(KeyCode::KEY_A))
                {
                    devices.push(KeyboardDevice { path, name });
                }
                 */
            }
            Err(error) => {
                // Ignore devices that cannot be opened
                eprintln!(
                    "Could not open device {}: {}",
                    entry.path().to_string_lossy(),
                    error
                );
                continue;
            } // Skip devices that cannot be opened
        }
    }

    return devices;
}
