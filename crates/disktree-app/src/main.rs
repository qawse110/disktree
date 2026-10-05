//! `disktree`: find what is eating a volume, mark it, and remove it.
//!
//! The window opens on a treemap of the scanned root — the home directory
//! unless another path is given — with a breadcrumb bar, a selection line, and a
//! live free-space meter. Marking is non-destructive until the review screen
//! is confirmed.

// A window, not a console program: on Windows, opening it from Explorer or
// the Start menu should not bring a console window along. `main` attaches to
// the console of a terminal it was started from, so `--help` and errors still
// reach one. Ignored elsewhere.
#![windows_subsystem = "windows"]

mod app_menu;
mod appearance;
mod git;
mod marks;
mod palette;
mod power;
mod state;
#[cfg(test)]
mod tests;
mod treemap_view;
mod ui;
mod views;
mod widgets;

use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::{
    io::IsTerminal as _,
    os::unix::process::CommandExt as _,
    process::{Command, Stdio},
};

use anyhow::{Context as _, Result};
use disktree_core::scan::ScanOptions;
use gpui_kit::{AppContext as _, WindowOptions, px, size};
use state::Disktree;

/// What the command line asked for.
#[derive(Debug)]
struct Args {
    root: PathBuf,
    options: ScanOptions,
    depth: u32,
    power: Option<power::PowerEfficiency>,
}

const USAGE: &str = "\
disktree — 用树状图看清磁盘空间被什么占用了

用法：disktree [选项] [路径]

参数：
  PATH              要扫描的目录（默认：主目录）

窗口以树状图显示扫描的根目录，最大的排在最前。空格键标记选中的图块，
回车进入，c 键复核已标记列表，? 键列出所有按键。

选项：
  -a, --apparent-size   按表观长度而非已分配块计量
  -l, --follow-links    跟随符号链接
  -H, --no-hidden       跳过点文件和点目录
  -D, --disk            扫描主目录所在的整个磁盘
  -X, --cross-filesystems
                        同时计量 PATH 下挂载的其他磁盘、网络共享和伪文件
                        系统（默认关闭）
  -d, --depth N         一次绘制的层数（1-6，默认 3）
      --power-efficiency PRESET
                        省电、均衡（默认）、激进、尽情耗电
      --scan-threads N  固定扫描线程数，以可用 CPU 数为上限
      --adaptive-threads
                        实验性的自适应准入（需显式开启）
      --fixed-threads   关闭自适应准入和 CPU 调节
      --thread-throughput-percent N
                        保留初始吞吐采样值的百分比（80）
      --thread-system-cpu-percent N
                        尽力遵守的主机 CPU 占用上限；0 表示不限制（80）
      --metric files    按文件数而非字节数排序
  -h, --help            显示此帮助
";

fn main() -> Result<()> {
    #[cfg(windows)]
    console::attach();
    let outcome = run();
    #[cfg(windows)]
    console::detach();
    outcome
}

fn run() -> Result<()> {
    let saved = power::settings_path()
        .map_or_else(
            || Ok(power::PowerEfficiency::default()),
            |path| power::load(&path),
        )
        .unwrap_or_else(|error| {
            eprintln!("无法加载电源效率设置：{error}；改用均衡模式");
            power::PowerEfficiency::default()
        });
    let args = parse_args_with_power(std::env::args_os().skip(1), saved)?;

    // When the app executable is reached through the command-line symlink,
    // cmux sends SIGTERM to its foreground process group as AppKit takes
    // focus. Spawn once into a separate group before AppKit starts. Restrict
    // this to interactive cmux sessions so scripts retain normal foreground
    // lifetime; the marker prevents the child from spawning recursively.
    #[cfg(target_os = "macos")]
    if std::io::stdin().is_terminal()
        && std::env::var_os("CMUX_SURFACE_ID").is_some()
        && std::env::var_os("DISKTREE_CMUX_DETACHED").is_none()
    {
        Command::new(std::env::current_exe().context("find disktree")?)
            .args(std::env::args_os().skip(1))
            .env("DISKTREE_CMUX_DETACHED", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // Zero makes the child the leader of a new process group.
            .process_group(0)
            .spawn()
            .context("start disktree")?;
        return Ok(());
    }

    let root = args.root.clone();
    let depth = args.depth;
    let title_root = root.clone();

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_omarchy::init(cx);
            app_menu::install(cx);
            let home = std::env::home_dir();
            let native_look = appearance::follows_system(home.as_deref());
            if native_look {
                appearance::apply(cx.window_appearance(), cx);
            }
            let options = args.options.clone();
            let root_for_app = root.clone();
            let window = cx
                .open_window(
                    WindowOptions {
                        window_bounds: Some(gpui_kit::WindowBounds::Windowed(
                            gpui_kit::Bounds::new(
                                gpui_kit::point(px(120.), px(90.)),
                                size(px(1440.), px(900.)),
                            ),
                        )),
                        titlebar: Some(gpui_kit::TitlebarOptions {
                            title: Some(
                                format!(
                                    "disktree · {}",
                                    marks::display_path(
                                        &title_root,
                                        home.as_deref(),
                                    )
                                )
                                .into(),
                            ),
                            ..Default::default()
                        }),
                        // Wayland app id. Hyprland reports it as the window
                        // class, and the desktop entry's StartupWMClass and
                        // the documented window rule both match `disktree`.
                        // Left unset, the class is empty and that rule never
                        // matches.
                        app_id: Some("disktree".to_owned()),
                        // Below this the treemap stops being readable, so ask
                        // the compositor not to go there.
                        window_min_size: Some(size(px(900.), px(600.))),
                        ..Default::default()
                    },
                    move |window, cx| {
                        if native_look {
                            appearance::follow(window);
                        }
                        cx.new(|cx| {
                            let mut app = Disktree::new(
                                root_for_app.clone(),
                                options.clone(),
                                depth,
                                cx,
                            );
                            app.power_choice = args.power;
                            app
                        })
                    },
                )
                .expect("open the disktree window");

            // The treemap owns the keyboard from the first frame; there is no
            // text field to focus first.
            let _ = window.update(cx, |this, window, cx| {
                let focus = this.focus.clone();
                window.focus(&focus, cx);
            });
            cx.activate(true);
        });
    Ok(())
}

/// Read the command line, program name already skipped.
#[cfg(test)]
fn parse_args(args: impl Iterator<Item = std::ffi::OsString>) -> Result<Args> {
    parse_args_with_power(args, power::PowerEfficiency::default())
}

fn parse_args_with_power(
    mut args: impl Iterator<Item = std::ffi::OsString>,
    preset: power::PowerEfficiency,
) -> Result<Args> {
    let mut root: Option<PathBuf> = None;
    let mut options = ScanOptions {
        threads: preset.policy(power::cpu_threads()),
        ..ScanOptions::default()
    };
    let mut power = Some(preset);
    let mut depth = 3_u32;
    let mut disk = false;
    // `std::env::args` panics on a name that is not Unicode, and a path is
    // any name: a restart as administrator hands the root back exactly as
    // it was, so the caller passes `args_os`.
    let text = |value: Option<std::ffi::OsString>, need: &str| {
        value
            .and_then(|value| value.into_string().ok())
            .with_context(|| need.to_owned())
    };

    while let Some(arg) = args.next() {
        match arg.to_str().unwrap_or_default() {
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "-a" | "--apparent-size" => options.apparent_size = true,
            "-l" | "--follow-links" => options.follow_links = true,
            "-H" | "--no-hidden" => options.include_hidden = false,
            // Staying on one volume is the default; the flag is kept so
            // old invocations still work.
            "-x" | "--one-filesystem" => options.one_filesystem = true,
            "-X" | "--cross-filesystems" => options.one_filesystem = false,
            "-D" | "--disk" => disk = true,
            "-d" | "--depth" => {
                let value = text(args.next(), "--depth 需要一个数字")?;
                depth = value.parse().context("--depth 需要一个数字")?;
                anyhow::ensure!(
                    (1..=6).contains(&depth),
                    "--depth 必须是 1 到 6"
                );
            }
            "--power-efficiency" => {
                let value =
                    text(args.next(), "--power-efficiency 需要一个预设值")?;
                let preset = power::PowerEfficiency::parse(&value)
                    .context("未知的电源效率预设值")?;
                options.threads = preset.policy(power::cpu_threads());
                power = Some(preset);
            }
            "--scan-threads" => {
                power = None;
                let value = text(args.next(), "--scan-threads 需要一个数字")?;
                options.threads.max_threads =
                    value.parse().context("--scan-threads 需要正整数")?;
                anyhow::ensure!(
                    options.threads.max_threads > 0,
                    "--scan-threads 必须为正数"
                );
            }
            "--fixed-threads" => {
                options.threads.adaptive = false;
                power = None;
            }
            "--adaptive-threads" => {
                options.threads.adaptive = true;
                options.threads.system_cpu_limit = Some(0.80);
                power = None;
            }
            "--thread-throughput-percent" => {
                let value = text(
                    args.next(),
                    "--thread-throughput-percent 需要一个数字",
                )?;
                let percent: u8 = value
                    .parse()
                    .context("吞吐率百分比必须在 1 到 100 之间")?;
                anyhow::ensure!(
                    (1..=100).contains(&percent),
                    "吞吐率百分比必须在 1 到 100 之间"
                );
                options.threads.retained_throughput =
                    f64::from(percent) / 100.0;
            }
            "--thread-system-cpu-percent" => {
                let value = text(
                    args.next(),
                    "--thread-system-cpu-percent 需要一个数字",
                )?;
                let percent: u8 =
                    value.parse().context("CPU 百分比必须在 0 到 100 之间")?;
                anyhow::ensure!(
                    percent <= 100,
                    "CPU 百分比必须在 0 到 100 之间"
                );
                options.threads.system_cpu_limit =
                    (percent > 0).then(|| f64::from(percent) / 100.0);
            }
            "--metric" => {
                let value = text(args.next(), "--metric 需要一个取值")?;
                options.metric = match value.as_str() {
                    "files" => disktree_core::tree::Metric::Files,
                    "bytes" | "size" => disktree_core::tree::Metric::Bytes,
                    other => anyhow::bail!(
                        "未知的计量方式 {other}；可用 bytes 或 files"
                    ),
                };
            }
            // Launch Services added a process serial number when opening an
            // app from Finder until OS X 10.9, and some launchers still do.
            other if other.starts_with("-psn_") => {}
            other if other.starts_with('-') => {
                anyhow::bail!("未知选项 {other}\n\n{USAGE}");
            }
            _ => {
                anyhow::ensure!(root.is_none(), "只能扫描一个路径");
                root = Some(PathBuf::from(arg));
            }
        }
    }

    anyhow::ensure!(!(disk && root.is_some()), "--disk 不能与 PATH 同时使用");
    let home = std::env::home_dir();
    let root = match root {
        _ if disk => home
            .as_deref()
            .and_then(disktree_core::space::volume_root_for)
            .unwrap_or_else(|| PathBuf::from("/")),
        Some(root) => root,
        None => home.context("没有给出路径，也找不到主目录")?,
    };
    // Store the depth as the initial view setting rather than a scan option: it
    // is a display choice the run-time `[` and `]` keys also change.
    // Canonical, so a later widening recognises this tree in the wider walk;
    // through dunce, so Windows gets `C:\Users\…` rather than the `\\?\C:\…`
    // form nothing else is written in.
    let root = dunce::canonicalize(&root).unwrap_or(root);
    let metadata = std::fs::metadata(&root)
        .with_context(|| format!("无法读取 {}", root.display()))?;
    anyhow::ensure!(metadata.is_dir(), "{} 不是目录", root.display());

    Ok(Args {
        root,
        options,
        depth: depth.clamp(1, 6),
        power,
    })
}

/// The console of the terminal disktree was started from, if any: a
/// windowed program on Windows gets none of its own.
#[cfg(windows)]
mod console {
    #![allow(
        unsafe_code,
        reason = "a few Win32 calls that take no pointers to get wrong"
    )]

    use std::sync::atomic::{AtomicU32, Ordering};

    use windows_sys::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, FreeConsole, GetConsoleOutputCP,
        SetConsoleOutputCP,
    };

    /// UTF-8. A Chinese console defaults to code page 936, which would
    /// turn the translated output into mojibake: the text is written as
    /// UTF-8 bytes either way.
    const UTF8_CODE_PAGE: u32 = 65_001;

    /// The code page found on attach, restored on detach. Zero while no
    /// console is borrowed.
    static SAVED_CODE_PAGE: AtomicU32 = AtomicU32::new(0);

    /// Borrow the parent's console so printed text reaches it, and put
    /// that console into UTF-8 for as long as we hold it. Does nothing
    /// when started from Explorer, which has none.
    pub fn attach() {
        // SAFETY: takes a process id by value, and failure only means
        // there was no console to attach to.
        unsafe {
            if AttachConsole(ATTACH_PARENT_PROCESS) != 0 {
                let previous = GetConsoleOutputCP();
                if previous != 0 && previous != UTF8_CODE_PAGE {
                    SAVED_CODE_PAGE.store(previous, Ordering::Relaxed);
                    SetConsoleOutputCP(UTF8_CODE_PAGE);
                }
            }
        }
    }

    /// Put the code page back and let go of the console again, so the
    /// shell redraws its prompt as it was.
    pub fn detach() {
        // SAFETY: no arguments; a process without a console is left as is.
        unsafe {
            let previous = SAVED_CODE_PAGE.swap(0, Ordering::Relaxed);
            if previous != 0 {
                SetConsoleOutputCP(previous);
            }
            FreeConsole();
        }
    }
}

#[cfg(test)]
mod argument_tests {
    use super::*;

    fn arguments(extra: &[&str]) -> impl Iterator<Item = std::ffi::OsString> {
        extra
            .iter()
            .copied()
            .chain(std::iter::once("."))
            .map(std::ffi::OsString::from)
    }

    #[test]
    fn adaptive_budget_and_fixed_override_are_explicit() {
        let args = parse_args(arguments(&[])).expect("defaults");
        assert_eq!(
            args.options.threads.max_threads,
            4.min(power::cpu_threads())
        );
        assert!(!args.options.threads.adaptive);
        let args = parse_args(arguments(&[
            "--scan-threads",
            "4",
            "--fixed-threads",
            "--thread-throughput-percent",
            "85",
            "--thread-system-cpu-percent",
            "0",
        ]))
        .expect("custom");
        assert_eq!(args.options.threads.max_threads, 4);
        assert!(!args.options.threads.adaptive);
        assert!(
            (args.options.threads.retained_throughput - 0.85).abs()
                < f64::EPSILON
        );
        assert_eq!(args.options.threads.system_cpu_limit, None);
    }

    #[test]
    fn saved_power_is_loaded_before_cli_overrides() {
        use power::PowerEfficiency as Power;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("power-efficiency");
        power::save(&path, Power::Miser).expect("save");
        let saved = power::load(&path).expect("load");
        let args = parse_args_with_power(arguments(&[]), saved).expect("saved");
        assert_eq!(args.power, Some(Power::Miser));
        assert_eq!(
            args.options.threads.max_threads,
            2.min(power::cpu_threads())
        );
        let args = parse_args_with_power(
            arguments(&["--power-efficiency", "drain-my-battery"]),
            saved,
        )
        .expect("override");
        assert_eq!(args.power, Some(Power::DrainMyBattery));
        assert_eq!(args.options.threads.max_threads, power::cpu_threads());
        let args = parse_args_with_power(
            arguments(&["--scan-threads", "8", "--adaptive-threads"]),
            saved,
        )
        .expect("experiment");
        assert!(args.power.is_none());
        assert!(args.options.threads.adaptive);
        assert_eq!(args.options.threads.max_threads, 8);
        assert_eq!(power::load(&path).expect("unchanged"), saved);
        assert!(
            parse_args(arguments(&["--power-efficiency", "invalid"])).is_err()
        );
    }

    // APFS rejects these filename bytes; Linux filesystems permit them.
    #[cfg(target_os = "linux")]
    #[test]
    fn worker_options_preserve_a_non_unicode_root() {
        use std::os::unix::ffi::OsStringExt as _;

        let temp = tempfile::TempDir::new().expect("tempdir");
        let root = temp
            .path()
            .join(std::ffi::OsString::from_vec(vec![b'r', 0xff]));
        std::fs::create_dir(&root).expect("directory");
        let args = parse_args(
            [
                std::ffi::OsString::from("--scan-threads"),
                std::ffi::OsString::from("4"),
                root.clone().into_os_string(),
            ]
            .into_iter(),
        )
        .expect("native path");
        assert_eq!(args.root, dunce::canonicalize(root).expect("root"));
        assert_eq!(args.options.threads.max_threads, 4);
    }

    #[test]
    fn invalid_worker_budgets_are_rejected() {
        for extra in [
            vec!["--scan-threads", "0"],
            vec!["--scan-threads", "-1"],
            vec!["--thread-throughput-percent", "0"],
            vec!["--thread-throughput-percent", "101"],
            vec!["--thread-system-cpu-percent", "101"],
        ] {
            assert!(parse_args(arguments(&extra)).is_err());
        }
    }
}
