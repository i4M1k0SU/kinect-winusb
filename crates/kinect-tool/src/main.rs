//! `kinect-winusb` — diagnostics and setup tool.

mod camtest;
mod descriptors;
mod devices;
mod hub;
mod nuitest;
mod probe;
#[cfg(target_pointer_width = "64")]
mod setup;

fn usage() -> ! {
    eprintln!(
        "kinect-winusb <command>\n\
         \n\
         commands:\n\
         \x20 probe                  list Kinect USB functions, drivers and camera descriptors\n\
         \x20 install                (admin) bind the camera to inbox WinUSB and register its interface\n\
         \x20 uninstall              (admin) remove WinUSB from the camera again\n\
         \x20 camtest [frames] [--restarts <n>]\n\
         \x20                        stream depth+color through the user-mode camera driver\n\
         \x20 nuitest [--dll <path>] [--seconds <n>]\n\
         \x20                        run the NUI API through Kinect10.dll (default: the proxy next to this exe)\n"
    );
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else { usage() };
    let rest = &args[1..];
    let code = match cmd.as_str() {
        "probe" => probe::run(rest),
        #[cfg(target_pointer_width = "64")]
        "install" => setup::install(rest),
        #[cfg(target_pointer_width = "64")]
        "uninstall" => setup::uninstall(rest),
        // SetupAPI refuses device installation from WOW64.
        #[cfg(target_pointer_width = "32")]
        "install" | "uninstall" => {
            eprintln!("{cmd} needs the 64-bit kinect-winusb.exe");
            1
        }
        "camtest" => camtest::run(rest),
        "nuitest" => nuitest::run(rest),
        _ => usage(),
    };
    std::process::exit(code);
}
