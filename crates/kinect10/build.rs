#[path = "../../build/windows.rs"]
mod windows;

fn main() {
    windows::compile("kinect10.dll", true);
}
