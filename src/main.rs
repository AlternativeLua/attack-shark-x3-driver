//! x3ctl: configure an Attack Shark X3 mouse from Linux.

mod hidraw;
mod protocol;
mod settings;

use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread::sleep;
use std::time::{Duration, Instant};

use hidraw::{DeviceInfo, Hidraw};
use protocol::{Event, Link, hex};
use settings::{Settings, group};

const USAGE: &str = "\
x3ctl - configure an Attack Shark X3 mouse

Usage: x3ctl [options] <command>

Commands:
  list                 Show the mice and dongles that were found
  read [--save]        Print the settings stored in the mouse
  set --<key> <value>  Change settings in the mouse, leaving the rest as is
  show                 Print the saved copy of the settings
  apply                Write the whole saved copy back to the mouse
  battery              Print the battery level (2.4G dongle only)
  monitor              Print events from the mouse until interrupted
  udev-rule            Print the udev rule that grants access without root

Settings for `set`:
  --dpi <dpi>          DPI of the active stage (50..26000, steps of 50)
  --dpi<N> <dpi>       DPI of stage N (1..6), e.g. --dpi3 3200
  --dpis <a,b,c,d,e,f> DPI of all six stages
  --stage <N>          Active DPI stage (1..6)
  --polling <hz>       125, 250, 500 or 1000
  --lod <mm>           Lift-off distance, 1 or 2
  --ripple <on|off>    Ripple control
  --angle-snap <on|off>
  --motion-sync <on|off>
  --sleep <min>        Idle time before sleep (0.5..30, steps of 0.5)
  --deep-sleep <min>   Idle time before deep sleep (1..60)
  --debounce <ms>      Key response time (4..50, even)

Options:
  -d, --device <dev>   wired, dongle, or a /dev/hidrawN path
                       (default: wired if plugged in, else the dongle)
  -c, --config <file>  Settings file (default: ~/.config/x3ctl/settings.conf)
  -n, --dry-run        Print the reports instead of sending them
  -v, --verbose        Show the raw bytes exchanged with the mouse
  -h, --help
  -V, --version";

const UDEV_RULE: &str = include_str!("../udev/70-x3ctl.rules");

/// How often a report is sent before giving up on it.
const SEND_ATTEMPTS: u32 = 3;
/// How long the mouse gets to acknowledge a report sent through the dongle.
const DONGLE_ACK_TIMEOUT: Duration = Duration::from_millis(800);
/// Over the cable the acknowledgement arrives within a few milliseconds.
const WIRED_ACK_WINDOW: Duration = Duration::from_millis(150);

#[derive(Default)]
struct Options {
    device: Option<String>,
    config: Option<PathBuf>,
    dry_run: bool,
    verbose: bool,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("x3ctl: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut opts = Options::default();
    let mut command = None;
    let mut rest = Vec::new();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            "-V" | "--version" => {
                println!("x3ctl {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "-n" | "--dry-run" => opts.dry_run = true,
            "-v" | "--verbose" => opts.verbose = true,
            "-d" | "--device" => opts.device = Some(value("--device")?),
            "-c" | "--config" => opts.config = Some(value("--config")?.into()),
            _ => {
                if let Some(dev) = arg.strip_prefix("--device=") {
                    opts.device = Some(dev.to_string());
                } else if let Some(path) = arg.strip_prefix("--config=") {
                    opts.config = Some(path.into());
                } else if command.is_none() && !arg.starts_with('-') {
                    command = Some(arg);
                } else {
                    rest.push(arg);
                }
            }
        }
    }

    let Some(command) = command else {
        return Err("no command given; see `x3ctl --help`".into());
    };
    if !matches!(command.as_str(), "set" | "read")
        && let Some(extra) = rest.first()
    {
        return Err(format!("{command}: unexpected argument `{extra}`"));
    }
    match command.as_str() {
        "list" => cmd_list(),
        "show" => cmd_show(&opts),
        "set" => cmd_set(&opts, &rest),
        "apply" => cmd_apply(&opts),
        "battery" => cmd_battery(&opts),
        "monitor" => cmd_monitor(&opts),
        "read" => cmd_read(&opts, &rest),
        "udev-rule" => {
            print!("{UDEV_RULE}");
            Ok(())
        }
        other => Err(format!("unknown command `{other}`; see `x3ctl --help`")),
    }
}

/// The config interfaces of every connected X3 or X3 dongle.
fn find_devices() -> Result<Vec<(DeviceInfo, Link)>, String> {
    let nodes = hidraw::scan(protocol::VENDOR_ID).map_err(|e| format!("/sys/class/hidraw: {e}"))?;
    Ok(nodes
        .into_iter()
        .filter(|d| d.feature_len(protocol::REPORT_SENSOR).is_some())
        .filter_map(|d| {
            let link = Link::from_product(d.product)?;
            Some((d, link))
        })
        .collect())
}

/// Picks the device named by `--device`, or else the first one on the
/// `preferred` link, or else whatever is there.
fn select(opts: &Options, preferred: Link) -> Result<(DeviceInfo, Link), String> {
    let mut devices = find_devices()?;
    if devices.is_empty() {
        return Err("no Attack Shark X3 found (looked for USB 1d57:fa61 and 1d57:fa60)".into());
    }
    let index = match opts.device.as_deref() {
        None => devices.iter().position(|d| d.1 == preferred).unwrap_or(0),
        Some(name) => {
            let wanted = match name {
                "wired" | "cable" | "usb" => Some(Link::Wired),
                "dongle" | "wireless" | "2.4g" => Some(Link::Dongle),
                _ => None,
            };
            devices
                .iter()
                .position(|d| match wanted {
                    Some(link) => d.1 == link,
                    None => d.0.node.as_os_str() == name,
                })
                .ok_or_else(|| match wanted {
                    Some(link) => format!("no {} X3 is connected", link.name()),
                    None => format!("{name} is not an X3 config interface; see `x3ctl list`"),
                })?
        }
    };
    Ok(devices.swap_remove(index))
}

fn open(opts: &Options, preferred: Link) -> Result<(Hidraw, Link), String> {
    let (info, link) = select(opts, preferred)?;
    let dev = Hidraw::open(&info).map_err(|e| open_error(&info, &e))?;
    if opts.verbose {
        eprintln!(
            "using {} ({}, {})",
            info.node.display(),
            info.name,
            link.name()
        );
    }
    // The wired X3 shares its USB ids with the R1, whose layout differs.
    if link == Link::Wired && !info.name.contains("X3") {
        eprintln!(
            "x3ctl: warning: {} calls itself `{}`, not an X3; its settings layout may differ",
            info.node.display(),
            info.name
        );
    }
    Ok((dev, link))
}

fn open_error(info: &DeviceInfo, err: &io::Error) -> String {
    let node = info.node.display();
    if err.kind() == io::ErrorKind::PermissionDenied {
        format!(
            "{node}: permission denied\n\
             Install the udev rule once:\n  \
             x3ctl udev-rule | sudo tee /etc/udev/rules.d/70-x3ctl.rules\n  \
             sudo udevadm control --reload\n  \
             sudo udevadm trigger --settle --subsystem-match=hidraw"
        )
    } else {
        format!("{node}: {err}")
    }
}

fn settings_path(opts: &Options) -> Result<PathBuf, String> {
    match &opts.config {
        Some(path) => Ok(path.clone()),
        None => settings::default_path(),
    }
}

fn cmd_list() -> Result<(), String> {
    let devices = find_devices()?;
    if devices.is_empty() {
        println!("no Attack Shark X3 found (looked for USB 1d57:fa61 and 1d57:fa60)");
        return Ok(());
    }
    for (info, link) in devices {
        let access = match Hidraw::open(&info) {
            Ok(_) => "ok".to_string(),
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                "no access (see `x3ctl udev-rule`)".to_string()
            }
            Err(e) => e.to_string(),
        };
        let interface = match info.interface {
            Some(n) => format!("if{n}"),
            None => "-".to_string(),
        };
        println!(
            "{:<15} {:<7} {:04x}:{:04x} {interface:<4} {:<28} {access}",
            info.node.display(),
            link.name(),
            info.vendor,
            info.product,
            info.name,
        );
    }
    Ok(())
}

fn cmd_show(opts: &Options) -> Result<(), String> {
    let path = settings_path(opts)?;
    match settings::load(&path)? {
        Some(saved) => println!("{}\n\nSaved in {}", saved.summary(), path.display()),
        None => println!("Nothing saved yet; `x3ctl read` shows what the mouse holds."),
    }
    Ok(())
}

/// Turns `--key value`, `--key=value` and `key=value` into pairs.
fn parse_pairs(args: &[String]) -> Result<Vec<(String, String)>, String> {
    let mut pairs = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let name = arg.trim_start_matches('-');
        let (key, value) = match name.split_once('=') {
            Some((key, value)) => (key, value.to_string()),
            None => match args.next() {
                Some(value) => (name, value.clone()),
                None => return Err(format!("{arg} needs a value")),
            },
        };
        pairs.push((key.to_string(), value));
    }
    Ok(pairs)
}

fn cmd_set(opts: &Options, args: &[String]) -> Result<(), String> {
    let mut pairs = parse_pairs(args)?;
    if pairs.is_empty() {
        return Err("set: nothing to change, e.g. `x3ctl set --dpi 1600`".into());
    }
    // `dpi` means "of the active stage", so it has to see a `stage` given
    // in the same command regardless of the order they were typed in.
    pairs.sort_by_key(|(key, _)| key == "dpi");
    let change = |s: &mut Settings| -> Result<u8, String> {
        let mut groups = 0;
        for (key, value) in &pairs {
            groups |= s.set(key, value).map_err(|e| format!("--{key}: {e}"))?;
        }
        Ok(groups)
    };
    // Reject bad values before anything is sent to the mouse.
    change(&mut Settings::default())?;

    let path = settings_path(opts)?;
    if opts.dry_run {
        // A dry run sends nothing, not even the request to read, so the
        // saved copy is the best starting point it has.
        let mut new = settings::load(&path)?.unwrap_or_default();
        let groups = change(&mut new)?;
        print_reports(opts, &new, groups);
        return Ok(());
    }

    let (dev, link) = open(opts, Link::Wired)?;
    // A report rewrites a whole block of settings, so the values that were
    // not asked for have to be the ones the mouse holds right now. The saved
    // copy goes stale whenever the DPI button or another tool is used.
    let mut new = match read_settings(&dev, opts.verbose) {
        Ok(current) => current,
        Err(e) => match settings::load(&path)? {
            Some(saved) => {
                eprintln!(
                    "x3ctl: note: could not read the mouse ({e}); starting from {} instead",
                    path.display()
                );
                saved
            }
            None => {
                return Err(format!(
                    "could not read the mouse's current settings ({e}) and there is no \
                     saved copy to start from, so nothing was written"
                ));
            }
        },
    };
    let groups = change(&mut new)?;
    send_reports(&dev, link, &new, groups, opts.verbose)?;
    settings::save(&path, &new)
}

fn cmd_apply(opts: &Options) -> Result<(), String> {
    let path = settings_path(opts)?;
    let Some(saved) = settings::load(&path)? else {
        return Err(format!(
            "{} does not exist yet; `x3ctl set` or `x3ctl read --save` creates it",
            path.display()
        ));
    };
    if opts.dry_run {
        print_reports(opts, &saved, group::ALL);
        return Ok(());
    }
    let (dev, link) = open(opts, Link::Wired)?;
    send_reports(&dev, link, &saved, group::ALL, opts.verbose)
}

/// The dry-run counterpart of [`send_reports`].
fn print_reports(opts: &Options, s: &Settings, groups: u8) {
    // Use the device that would be written to, if there is one, so the
    // padding shown is the padding that would be sent.
    let link = match (select(opts, Link::Wired), opts.device.as_deref()) {
        (Ok((_, link)), _) => link,
        (Err(_), Some("dongle" | "wireless" | "2.4g")) => Link::Dongle,
        (Err(_), _) => Link::Wired,
    };
    for (what, frame) in protocol::frames(s, groups) {
        let wire = protocol::padded(&frame, link);
        println!("{what} ({}): {}", link.name(), hex(&wire));
    }
}

fn send_reports(
    dev: &Hidraw,
    link: Link,
    s: &Settings,
    groups: u8,
    verbose: bool,
) -> Result<(), String> {
    for (what, frame) in protocol::frames(s, groups) {
        let acked = send(dev, link, &frame, verbose).map_err(|e| format!("{what}: {e}"))?;
        println!(
            "{what} written{}",
            if acked { ", acknowledged" } else { "" }
        );
    }
    Ok(())
}

/// Sends one report and waits for the mouse to acknowledge it, resending if
/// it is rejected or, over the dongle, goes unanswered. Returns whether an
/// acknowledgement was seen; over the cable its absence is tolerated.
fn send(dev: &Hidraw, link: Link, frame: &[u8], verbose: bool) -> Result<bool, String> {
    let wire = protocol::padded(frame, link);
    let id = frame[0];
    let timeout = match link {
        Link::Wired => WIRED_ACK_WINDOW,
        Link::Dongle => DONGLE_ACK_TIMEOUT,
    };

    let mut rejected = None;
    for attempt in 1..=SEND_ATTEMPTS {
        dev.drain().map_err(|e| e.to_string())?;
        if verbose {
            eprintln!("-> {}", hex(&wire));
        }
        dev.set_feature(&wire)
            .map_err(|e| format!("SET_REPORT failed: {e}"))?;

        let status = wait_for(dev, timeout, verbose, |event| match event {
            Event::Ack { status, report } if report == id => Some(status),
            _ => None,
        })?;
        match status {
            Some(0) => return Ok(true),
            Some(status) => rejected = Some(status),
            None if link == Link::Wired => return Ok(false),
            None => {}
        }
        if verbose {
            eprintln!("not accepted (attempt {attempt}/{SEND_ATTEMPTS})");
        }
    }
    Err(match rejected {
        Some(status) => format!("the mouse rejected report {id:#04x} (status {status:#04x})"),
        None => "the dongle took the report but the mouse never acknowledged it; \
                 is it switched to 2.4G, awake and in range?"
            .into(),
    })
}

/// Reads input reports until `pick` accepts one or `timeout` runs out.
fn wait_for<T>(
    dev: &Hidraw,
    timeout: Duration,
    verbose: bool,
    pick: impl Fn(Event) -> Option<T>,
) -> Result<Option<T>, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let Some(raw) = dev.read_input(left).map_err(|e| e.to_string())? else {
            return Ok(None);
        };
        if verbose {
            eprintln!("<- {}", hex(&raw));
        }
        if let Some(found) = Event::parse(&raw).and_then(&pick) {
            return Ok(Some(found));
        }
        if left.is_zero() {
            return Ok(None);
        }
    }
}

fn describe_battery(state: u8, level: u8) -> String {
    let percent = protocol::battery_percent(level);
    match protocol::battery_state(state) {
        Some(state) => format!("{percent}% ({state})"),
        None => format!("{percent}% (state {state:#04x})"),
    }
}

fn cmd_battery(opts: &Options) -> Result<(), String> {
    let (dev, link) = open(opts, Link::Dongle)?;
    if link == Link::Wired {
        return Err("the battery level is only reported over the 2.4G dongle; \
                    on the cable the mouse is charging"
            .into());
    }
    // The mouse broadcasts its level every couple of seconds while linked.
    let battery = wait_for(
        &dev,
        Duration::from_secs(6),
        opts.verbose,
        |event| match event {
            Event::Battery { state, level } => Some((state, level)),
            _ => None,
        },
    )?;
    match battery {
        Some((state, level)) => {
            println!("{}", describe_battery(state, level));
            Ok(())
        }
        None => {
            Err("no battery report within 6 s; is the mouse switched to 2.4G and awake?".into())
        }
    }
}

fn cmd_monitor(opts: &Options) -> Result<(), String> {
    let (dev, _) = open(opts, Link::Wired)?;
    eprintln!("listening; press the DPI button or wait for a battery report (Ctrl-C to stop)");
    loop {
        let Some(raw) = dev
            .read_input(Duration::from_secs(3600))
            .map_err(|e| e.to_string())?
        else {
            continue;
        };
        if opts.verbose {
            eprintln!("<- {}", hex(&raw));
        }
        match Event::parse(&raw) {
            Some(Event::DpiStage(stage)) => println!("dpi stage {stage}"),
            Some(Event::Battery { state, level }) => {
                println!("battery {}", describe_battery(state, level))
            }
            Some(Event::Ack { status, report }) => {
                println!("ack report {report:#04x} status {status:#04x}")
            }
            Some(Event::Other { code, a, b }) => {
                println!("event {code:#04x} {a:#04x} {b:#04x}")
            }
            // Reports 1 and 2 on this interface are ordinary system and
            // consumer keys; the kernel already handles those.
            None => {}
        }
    }
}

/// Asks the mouse for one of its stored reports.
///
/// The firmware answers a plain GET_REPORT only after report 0xa0 has armed
/// it, and each arming is good for a single read.
fn query(dev: &Hidraw, report: u8, len: u8, verbose: bool) -> Result<Vec<u8>, String> {
    let request = protocol::query_request(report, len);
    for _ in 0..10 {
        if verbose {
            eprintln!("-> {}", hex(&request));
        }
        dev.set_feature(&request)
            .map_err(|e| format!("arming the read failed: {e}"))?;
        sleep(Duration::from_millis(60));

        // A firmware without this handshake stalls or times out here (the
        // kernel waits 5 s), so an error is final rather than retried.
        let armed = dev
            .get_feature(protocol::REPORT_QUERY, request.len())
            .map_err(|e| format!("the mouse does not answer feature reads: {e}"))?;
        if verbose {
            eprintln!("<- {}", hex(&armed));
        }
        if armed.get(1) == Some(&1) {
            sleep(Duration::from_millis(50));
            let data = dev
                .get_feature(report, usize::from(len))
                .map_err(|e| format!("GET_REPORT failed: {e}"))?;
            if verbose {
                eprintln!("<- {}", hex(&data));
            }
            return Ok(data);
        }
        sleep(Duration::from_millis(60));
    }
    Err("the mouse did not answer the read request; is it awake, \
         and switched to 2.4G if this is the dongle?"
        .into())
}

/// Reads all three settings blocks from the mouse.
fn read_settings(dev: &Hidraw, verbose: bool) -> Result<Settings, String> {
    type Decode = fn(&[u8], &mut Settings) -> Result<(), String>;
    // The length asked for is the one in each report's own header.
    let reports: [(&str, u8, u8, Decode); 3] = [
        (
            "sensor settings",
            protocol::REPORT_SENSOR,
            0x38,
            protocol::decode_sensor,
        ),
        (
            "polling rate",
            protocol::REPORT_POLLING,
            0x09,
            protocol::decode_polling,
        ),
        (
            "power settings",
            protocol::REPORT_POWER,
            0x0f,
            protocol::decode_power,
        ),
    ];
    let mut settings = Settings::default();
    for (what, report, len, decode) in reports {
        let raw = query(dev, report, len, verbose).map_err(|e| format!("{what}: {e}"))?;
        decode(&raw, &mut settings).map_err(|e| format!("{what}: {e}; raw: {}", hex(&raw)))?;
    }
    Ok(settings)
}

fn cmd_read(opts: &Options, args: &[String]) -> Result<(), String> {
    let save = match args {
        [] => false,
        [flag] if flag == "--save" => true,
        [other, ..] => return Err(format!("read: unexpected argument `{other}`")),
    };
    let (dev, _) = open(opts, Link::Wired)?;
    let current = read_settings(&dev, opts.verbose)?;
    println!("{}", current.summary());
    if save {
        let path = settings_path(opts)?;
        settings::save(&path, &current)?;
        println!("\nSaved in {}", path.display());
    }
    Ok(())
}
