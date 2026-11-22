#[path = "../../build/windows.rs"]
mod windows;

fn main() {
    windows::compile("kinect-winusb.exe", false);
}
