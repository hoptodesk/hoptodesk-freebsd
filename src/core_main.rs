use hbb_common::{config, log};
#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux", target_os = "freebsd"))]
use std::fs;
#[cfg(target_os = "windows")]
use std::process::{id};
					
use hbb_common::{config::{Config},};

lazy_static::lazy_static! {
    pub(crate) static ref SWITCH_SIDES_INVOKED_UUID: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
}

#[macro_export]
macro_rules! my_println{
    ($($arg:tt)*) => {
        #[cfg(not(windows))]
        println!("{}", format_args!($($arg)*));
        #[cfg(windows)]
        crate::platform::message_box(
            &format!("{}", format_args!($($arg)*))
        );
    };
}

/// shared by flutter and sciter main function
///
/// [Note]
/// If it returns [`None`], then the process will terminate, and flutter gui will not be started.
/// If it returns [`Some`], then the process will continue, and flutter gui will be started.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn core_main() -> Option<Vec<String>> {
    // FreeBSD: vendored OpenSSL needs CA cert path for TLS
    #[cfg(target_os = "freebsd")]
    {
        if std::env::var("SSL_CERT_FILE").is_err() {
            for path in &["/usr/local/etc/ssl/cert.pem", "/usr/local/share/certs/ca-root-nss.crt", "/etc/ssl/cert.pem"] {
                if std::path::Path::new(path).exists() {
                    std::env::set_var("SSL_CERT_FILE", path);
                    break;
                }
            }
        }
    }

    let mut args: Vec<String> = Vec::new();
    let mut flutter_args: Vec<String> = Vec::new();

    let mut _is_elevate = false;
    let mut _is_run_as_system = false;
    let mut _is_quick_support = false;
    let mut _is_flutter_invoke_new_connection = false;
    let mut no_server = false;
    let mut arg_exe = Default::default();

    let mut env_args_iter = std::env::args().peekable();
    let mut arg_idx = 0;

    while let Some(current_arg) = env_args_iter.next() {
        if arg_idx == 0 {
            arg_exe = current_arg.clone();
            arg_idx += 1;
            continue;
        }

        let mut arg_handled_as_switch_uuid = false;

        if current_arg == "--switch-uuid" || current_arg == "--switch_uuid" {
            if let Some(uuid_peek_val) = env_args_iter.peek() {
                if !uuid_peek_val.starts_with("--") {
                    let uuid_value = env_args_iter.next().unwrap();
                    if let Ok(mut global_uuid_lock) = SWITCH_SIDES_INVOKED_UUID.lock() {
                        *global_uuid_lock = Some(uuid_value);
                        log::info!("CLI: Parsed --switch-uuid with value: {:?}", *global_uuid_lock);
                    } else {
                        log::error!("CLI: Failed to lock SWITCH_SIDES_INVOKED_UUID for writing.");
                    }
                    arg_handled_as_switch_uuid = true;
                } else {
                    log::warn!("CLI: --switch-uuid found, but the next token ('{}') looks like another flag, not a value.", uuid_peek_val);
                }
            } else {
                log::warn!("CLI: --switch-uuid found as the last argument without a value.");
            }
        }

        if arg_handled_as_switch_uuid {
            // If --switch-uuid (and its value) was processed, it should not be pushed to `args`
            // and no further processing for this argument in the old logic path.
        } else {
            let mut is_special_bool_flag_not_pushed_to_args = false;

            #[cfg(feature = "flutter")]
            if [
                "--connect",
                "--play",
                "--file-transfer",
                "--view-camera",
                "--port-forward",
                "--rdp",
                "--select-for-print",
            ]
            .contains(&current_arg.as_str())
            {
                _is_flutter_invoke_new_connection = true;
            }

            if current_arg == "--elevate" {
                _is_elevate = true;
                is_special_bool_flag_not_pushed_to_args = true;
            } else if current_arg == "--run-as-system" {
                _is_run_as_system = true;
                is_special_bool_flag_not_pushed_to_args = true;
            } else if current_arg == "--quick_support" {
                _is_quick_support = true;
                is_special_bool_flag_not_pushed_to_args = true;
            } else if current_arg == "--no-server" {
                no_server = true;
                is_special_bool_flag_not_pushed_to_args = true;
            }

            if !is_special_bool_flag_not_pushed_to_args {
                args.push(current_arg.clone());
            }
        }
        arg_idx += 1;
    }

    let click_setup = cfg!(windows) && args.is_empty() && crate::common::is_setup(&arg_exe);
    if click_setup && !config::is_disable_installation() {
        args.push("--install".to_owned());
        flutter_args.push("--install".to_string());
    }
    // Auto-upgrade: if running a newer version than what's installed as the service,
    // trigger the install flow to replace the old service binary.
    #[cfg(windows)]
    if args.is_empty() && !click_setup && !config::is_disable_installation() {
        if crate::platform::is_installed() {
            let installed_version = crate::platform::get_installed_version();
            let cur = hbb_common::get_version_number(crate::VERSION);
            let inst = hbb_common::get_version_number(&installed_version);
            if cur > inst {
                // Also check we're not already running from the installed path
                let (_, _, _, installed_exe, _) = crate::platform::get_install_info();
                let current_exe = std::env::current_exe().unwrap_or_default();
                if current_exe != std::path::PathBuf::from(&installed_exe) {
                    log::info!(
                        "Auto-upgrade: running version {} > installed version {}, triggering install",
                        crate::VERSION, installed_version
                    );
                    args.push("--install".to_owned());
                    flutter_args.push("--install".to_string());
                }
            }
        }
    }
    if args.contains(&"--noinstall".to_string()) {
        args.clear();
    }
    // Attach to parent console early for --mcp stdio mode on Windows.
    // Must happen before any Rust I/O so stdin/stdout handles are valid.
    #[cfg(windows)]
    if args.contains(&"--mcp".to_string()) {
        crate::platform::attach_console_for_stdio();
    }
    if args.len() > 0 && args[0] == "--version" {
        println!("{}", crate::VERSION);
        return None;
    }
    let mut log_name = "".to_owned();
    if args.len() > 0 && args[0].starts_with("--") {
        let name = args[0].replace("--", "");
        if !name.is_empty() {
            log_name = name;
        }
    }
    hbb_common::init_log(false, &log_name);

    // linux uni (url) go here.
    #[cfg(all(target_os = "linux", feature = "flutter"))]
    if args.len() > 0 && args[0].starts_with("hoptodesk:") {
        return try_send_by_dbus(args[0].clone());
    }

    #[cfg(windows)]
    if !crate::platform::is_installed()
        && args.is_empty()
        && _is_quick_support
        && !_is_elevate
        && !_is_run_as_system
    {
        use crate::portable_service::client;
        if let Err(e) = client::start_portable_service(client::StartPara::Direct) {
            log::error!("Failed to start portable service:{:?}", e);
        }
    }
    #[cfg(windows)]
    if !crate::platform::is_installed() && (_is_elevate || _is_run_as_system) {
        crate::platform::elevate_or_run_as_system(click_setup, _is_elevate, _is_run_as_system);
        return None;
    }
    if args.is_empty()
		|| args[0] == "--qs"
		|| crate::common::is_empty_uni_link(&args[0])
		|| std::env::current_exe().ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().contains("-qs"))).unwrap_or(false) {
        std::thread::spawn(move || crate::start_server(false, no_server));
        // Start dashboard WebSocket connection (works in both portable and installed mode)
        if crate::dashboard::is_linked() || !crate::dashboard::get_invite_code().is_empty() {
            std::thread::spawn(|| crate::dashboard::start());
        }
        // Start local MCP WebSocket server (localhost-only, auth token required)
        // Not needed on Linux where HopToDesk typically runs as a headless service
        #[cfg(not(target_os = "linux"))]
        {
            let port: u16 = crate::ui_interface::get_option("mcp-ws-port")
                .parse()
                .unwrap_or(9333);
            std::thread::spawn(move || crate::mcp_server::run_ws_local(port));
        }
    } else {
        #[cfg(windows)]
        {
            use crate::platform;
            if args[0] == "--uninstall" {
                if let Err(err) = platform::uninstall_me(true) {
                    log::error!("Failed to uninstall: {}", err);
                }
                return None;
            } else if args[0] == "--after-install" {
                if let Err(err) = platform::run_after_install() {
                    log::error!("Failed to after-install: {}", err);
                }
                return None;
            } else if args[0] == "--before-uninstall" {
                if let Err(err) = platform::run_before_uninstall() {
                    log::error!("Failed to before-uninstall: {}", err);
                }
                return None;
            } else if args[0] == "--silent-install" {
				hbb_common::allow_err!(platform::install_me(
                    "desktopicon startmenu",
                    "".to_owned(),
                    true,
                    args.len() > 1,
					false
                ));
                return None;
            } else if args[0] == "--silent-install-noshortcuts" {
                hbb_common::allow_err!(platform::install_me(
                    "",
                    "".to_owned(),
                    true,
                    args.len() > 2 || (args.len() > 1 && args.get(1) != Some(&"--nostartup".to_owned())),
                    args.get(1) == Some(&"--nostartup".to_owned())
                ));
                return None;						
            } else if args[0] == "--extract" {
                #[cfg(feature = "with_rc")]
                hbb_common::allow_err!(crate::rc::extract_resources(&args[1]));
                return None;
            } else if args[0] == "--tray" {
                crate::tray::start_tray();
                return None;
            } else if args[0] == "--portable-service" {
                crate::platform::elevate_or_run_as_system(
                    click_setup,
                    _is_elevate,
                    _is_run_as_system,
                );
                return None;
            }
        }
		if args[0] == "--connect" {
			let input = &args[1];
				
			if input != "hoptodesk:///" {

				if input.starts_with("hoptodesk://connect/") {

					let id_with_password = input.strip_prefix("hoptodesk://connect/").unwrap();
					let mut new_args = args.clone();  // Create a new vector to modify

					let mut parts = id_with_password.splitn(4, '/');
					if let Some(id) = parts.next().map(str::to_owned) {
						new_args[1] = id.to_string();
					}
					if let Some(password) = parts.next() {
						new_args.push(password.to_owned());
						
					}


					let config_path = Config::path("TeamID.toml");
					
					#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux", target_os = "freebsd"))]
					if let Some(parent_dir) = config_path.parent() {
						if !parent_dir.exists() {
							fs::create_dir_all(parent_dir)
								.expect("Failed to create directory for TeamID.toml");
						}
					}
					if let Some(teamid) = parts.next() {
						if teamid.len() == 16 || teamid.len() == 32 {
							fs::write(&Config::path("TeamID.toml"), teamid).expect("Failed to write TeamID to file");
						}
					}					
					if let Some(tokenex) = parts.next() {
						fs::write(&Config::path("LastToken.toml"), tokenex).expect("Failed to write tokenex to file");
					}					
					
					args = new_args;  // Assign the modified vector back to args
				} else if input.starts_with("hoptodesk://filetransfer/") {
                    log::info!("Staring file transfer...");
					if let Some(id) = input.strip_prefix("hoptodesk://filetransfer/").map(str::to_owned) {
						args[1] = id.to_string();
						args[0] = "--file-transfer".to_string();
					}
				} else if input.starts_with("hoptodesk://sync/") {
					log::info!("Staring sync...");
					if let Some(id) = input.strip_prefix("hoptodesk://sync/").map(str::to_owned) {
						args[1] = id.to_string();
						if args[1].is_empty() {
							hbb_common::config::Config::set_option("custom-api-url".to_owned(), "".to_owned());
						} else {
							hbb_common::config::Config::set_option("custom-api-url".to_owned(), format!("https://api.hoptodesk.com/?n={}", args[1]));
						}
						std::process::exit(0);
					}
				}
			}
		}
		if args[0] == "--remove" {
            if args.len() == 2 {
                // sleep a while so that process of removed exe exit
                std::thread::sleep(std::time::Duration::from_secs(1));
                std::fs::remove_file(&args[1]).ok();
                return None;
            }
        } else if args[0] == "--tray" {
            if !crate::check_process("--tray", true) {
                crate::tray::start_tray();
            }
            return None;
        } else if args[0] == "--changeid" {
			let config_path = Config::path("HopToDesk.toml");
			if let Ok(_metadata) = fs::metadata(&config_path) {
				let content = std::fs::read_to_string(&config_path).unwrap_or_else(|err| {
					log::error!(
						"Error reading file: {:?}({})",
						config_path.to_str(),
						err
					);
					String::new()
				});

				let filtered_content: String = content
					.lines()
					.filter(|line| !line.starts_with("enc_id = ") && !line.starts_with("salt = "))
					.map(|line| format!("{}\n", line))
					.collect();

				if let Err(err) = fs::write(&config_path, filtered_content) {
					log::error!("Error writing file: {:?}({})", config_path.to_str(), err);
				} else {
					log::info!("ID changed.");
				}
			}
        } else if args[0] == "--remoteupdate" {
			#[cfg(windows)]
			{
				let mut remoteupdate_arg = vec![String::from("--remoteupdate")];
				log::info!("Run with --remoteupdate");
				crate::ui::start(&mut remoteupdate_arg);
				std::process::exit(0);
			}
        } else if cfg!(windows) && (args[0] == "--update" || args[0] == "--updatefromremote") {
		    log::info!("Updating from --update");
			let exe_path = std::env::current_exe().expect("Failed to get current executable path");
		    #[cfg(windows)]
		    if let Ok(_metadata) = fs::metadata(Config::path("UpdatePath.toml")) {
		        log::info!("UpdatePath found");
				let lastpath = std::fs::read_to_string(Config::path("UpdatePath.toml")).unwrap_or_else(|err| {
		            log::error!(
		                "Error reading file: {:?}({})",
		                Config::path("UpdatePath.toml").to_str(),
		                err
		            );
		            String::new()
		        });
		
		        if crate::platform::is_installed() {
		            let (subkey, mut path, _start_menu, _, _) = crate::platform::windows::get_install_info();
		            path.push_str("\\HopToDesk.exe");
		            
		            for cmd in &[
		                ("sc", "stop HopToDesk"),
		                ("taskkill", &format!("/F /IM {:?}.exe", "HopToDesk")),
		                ("reg", &format!("add {} /f /v DisplayVersion /t REG_SZ /d \"{}\"", subkey, crate::VERSION)),
		                ("reg", &format!("add {} /f /v Version /t REG_SZ /d \"{}\"", subkey, crate::VERSION))
		            ] {
		                let _ = crate::platform::windows::run_uac_hide(cmd.0, cmd.1);
		            }
		
		            std::thread::sleep(std::time::Duration::from_secs(10));
		            
		            if let Err(err) = fs::remove_file(&path) {
		                log::error!("Failed to remove file: {}. Error: {}", path, err);
		            }
		
		            if let Err(err) = fs::copy(&exe_path, &path) {
		                log::error!("Failed to copy file to path: {}. Error: {}", path, err);
		            }
		
		            std::thread::sleep(std::time::Duration::from_secs(1));
		            let _ = crate::platform::windows::run_uac_hide("sc", "start HopToDesk");
		        } else {
					log::info!("Updating exe... not installed.");
					let current_pid = id();
					if let Err(err) = crate::platform::windows::run_uac_hide("taskkill", &format!("/F /IM {:?}.exe /FI \"PID ne {:?}\"", "HopToDesk", current_pid)) {
						log::error!("Failed to kill task: HopToDesk. Error: {}", err);
					} else {
						std::thread::sleep(std::time::Duration::from_secs(2));
						if let Err(err) = fs::remove_file(&lastpath) {
							log::error!("Failed to remove file: {}. Error: {}", lastpath, err);
						} else {
							log::info!("Removed old version {}", lastpath);
						}
		
						if let Err(err) = fs::copy(&exe_path, &lastpath) {
							log::error!("Failed to copy file to last path: {}. Error: {}", lastpath, err);
						} else {
							log::info!("Copied new version from {:?} to {:?}", exe_path, lastpath);
						}
						log::info!("Running new version {}", lastpath);
						let _ = crate::platform::windows::run_uac(&lastpath, "");
					}
				}
				
		        if crate::platform::is_installed() {		
		            let _ = crate::platform::windows::run_uac_hide("sc", "start HopToDesk");
		        }
		
		        std::thread::sleep(std::time::Duration::from_secs(5));
		    }
		    std::process::exit(0);
		} else if args[0] == "--install-service" {
            log::info!("start --install-service");
            crate::platform::install_service();
            return None;
        } else if args[0] == "--uninstall-service" {
            log::info!("start --uninstall-service");
            crate::platform::uninstall_service(false, true);
            return None;
        } else if args[0] == "--service" {
            log::info!("start --service");
            crate::start_os_service();
            return None;
        } else if args[0] == "--server" {
            log::info!("start --server with user {}", crate::username());
            // Start dashboard WebSocket alongside the signal server
            if crate::dashboard::is_linked() || !crate::dashboard::get_invite_code().is_empty() {
                std::thread::spawn(|| crate::dashboard::start());
            }
            // Start local MCP WebSocket server (same as portable mode)
            // Not needed on Linux where HopToDesk typically runs as a headless service
            #[cfg(not(target_os = "linux"))]
            {
                let port: u16 = crate::ui_interface::get_option("mcp-ws-port")
                    .parse()
                    .unwrap_or(9333);
                std::thread::spawn(move || crate::mcp_server::run_ws_local(port));
            }
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            {
                hbb_common::allow_err!(crate::platform::check_autostart_config());
                std::process::Command::new("pkill")
                    .arg("-f")
                    .arg(&format!("{} --tray", crate::get_app_name().to_lowercase()))
                    .status()
                    .ok();
                hbb_common::allow_err!(crate::platform::run_as_user(
                    vec!["--tray"],
                    None,
                    None::<(&str, &str)>,
                ));
            }
            #[cfg(windows)]
            crate::privacy_mode::restore_reg_connectivity(true, false);
            #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "windows"))]
            {
                crate::start_server(true, false);
            }
            #[cfg(target_os = "macos")]
            {
                let handler = std::thread::spawn(move || crate::start_server(true, false));
                crate::tray::start_tray();
                // prevent server exit when encountering errors from tray
                hbb_common::allow_err!(handler.join());
            }
            return None;
        } else if args[0] == "--import-config" {
			if args.len() == 2 {
                let filepath;
                let path = std::path::Path::new(&args[1]);
                if !path.is_absolute() {
					let mut cur = std::env::current_dir().unwrap();
                    cur.push(path);
                    filepath = cur.to_str().unwrap().to_string();
                } else {
					filepath = path.to_str().unwrap().to_string();
                }
				import_config(&filepath);
            }
            return None;
        } else if args[0] == "--password" {
            if args.len() == 2 {
                if crate::platform::is_installed() && is_root() {
                    if let Err(err) = crate::ipc::set_permanent_password(args[1].to_owned()) {
                        println!("{err}");
                    } else {
                        println!("Done!");
                    }
                } else {
                    println!("Installation and administrative privileges required!");
                }
            }
            return None;
        } else if args[0] == "--mcp" {
            crate::mcp_server::run();
            return None;
        } else if args[0] == "--mcp-port" {
            let port: u16 = args.get(1)
                .and_then(|s| s.parse().ok())
                .unwrap_or(9222);
            crate::mcp_server::run_tcp(port);
            return None;
        } else if args[0] == "--get-id" {
			println!("{}", crate::ipc::get_id());
			return None;
        } else if args[0] == "--check-hwcodec-config" {
            #[cfg(feature = "hwcodec")]
            crate::ipc::hwcodec_process();
            return None;
        } else if args[0] == "--cm" {
            // call connection manager to establish connections
            // meanwhile, return true to call flutter window to show control panel
            crate::ui_interface::start_option_status_sync();
        } else if args[0] == "--cm-no-ui" {
            #[cfg(feature = "flutter")]
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            {
                //crate::ui_interface::start_option_status_sync();
                crate::flutter::connection_manager::start_cm_no_ui();
            }
            return None;
        } else if args[0] == "--whiteboard" {
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            {
                crate::whiteboard::run();
            }
            return None;
        } else if args[0] == "-gtk-sudo" {
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            if args.len() > 2 {
                crate::platform::gtk_sudo::exec();
            }
            return None;
        } else if args[0] == "--ticket" {
            if crate::dashboard::is_linked() || !crate::dashboard::get_invite_code().is_empty() {
                std::thread::spawn(|| crate::dashboard::start());
            }
        } else {
            /*#[cfg(all(feature = "flutter", feature = "plugin_framework"))]
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            if args[0] == "--plugin-install" {
                if args.len() == 2 {
                    crate::plugin::change_uninstall_plugin(&args[1], false);
                } else if args.len() == 3 {
                    crate::plugin::install_plugin_with_url(&args[1], &args[2]);
                }
                return None;
            } else if args[0] == "--plugin-uninstall" {
                if args.len() == 2 {
                    crate::plugin::change_uninstall_plugin(&args[1], true);
                }
                return None;
            }*/
        }
    }
    //_async_logger_holder.map(|x| x.flush());
    #[cfg(feature = "flutter")]
    return Some(flutter_args);
    #[cfg(not(feature = "flutter"))]
    return Some(args);
}

/*#[inline]
#[cfg(all(feature = "flutter", feature = "plugin_framework"))]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn init_plugins(args: &Vec<String>) {
    if args.is_empty() || "--server" == (&args[0] as &str) {
        #[cfg(debug_assertions)]
        let load_plugins = true;
        #[cfg(not(debug_assertions))]
        let load_plugins = crate::platform::is_installed();
        if load_plugins {
            crate::plugin::init();
        }
    } else if "--service" == (&args[0] as &str) {
        hbb_common::allow_err!(crate::plugin::remove_uninstalled());
    }
}*/

fn import_config(path: &str) {
    use hbb_common::{config::*, get_exe_time, get_modified_time};
    let path2 = path.replace(".toml", "2.toml");
    let path2 = std::path::Path::new(&path2);
    let path = std::path::Path::new(path);
    log::info!("import config from {:?} and {:?}", path, path2);
    let config: Config = load_path(path.into());
    if config.is_empty() {
        log::info!("Empty source config, skipped");
        return;
    }
    if get_modified_time(&path) > get_modified_time(&Config::file())
        && get_modified_time(&path) < get_exe_time()
    {
        if store_path(Config::file(), config).is_err() {
            log::info!("config written");
        }
    }
    let config2: Config2 = load_path(path2.into());
    if get_modified_time(&path2) > get_modified_time(&Config2::file()) {
        if store_path(Config2::file(), config2).is_err() {
            log::info!("config2 written");
        }
    }
}

#[cfg(all(target_os = "linux", feature = "flutter"))]
fn try_send_by_dbus(uni_links: String) -> Option<Vec<String>> {
    use crate::dbus::invoke_new_connection;

    match invoke_new_connection(uni_links) {
        Ok(()) => {
            return None;
        }
        Err(err) => {
            log::error!("{}", err.as_ref());
            return Some(Vec::new());
        }
    }
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn is_root() -> bool {
    #[cfg(windows)]
    {
        return crate::platform::is_elevated(None).unwrap_or_default()
            || crate::platform::is_root();
    }
    #[allow(unreachable_code)]
    crate::platform::is_root()
}
