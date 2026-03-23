#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use std::env;
use std::fs::{self, File};
use std::io::Read;
use std::path::PathBuf;
use std::ffi::{CString, OsString};
use std::os::windows::ffi::OsStringExt;
use std::ptr::null_mut;
use winapi::um::libloaderapi::{GetModuleHandleA, GetProcAddress};
use winapi::um::memoryapi::{VirtualAllocEx, WriteProcessMemory};
use winapi::um::processthreadsapi::{OpenProcess};
use winapi::um::psapi::{EnumProcesses, GetProcessImageFileNameA};
use winapi::um::winnt::{MEM_COMMIT, PAGE_READWRITE, PROCESS_ALL_ACCESS, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ};
use winapi::shared::minwindef::{DWORD, MAX_PATH};
use winapi::um::errhandlingapi::GetLastError;
use winapi::um::winbase::INFINITE;
use winapi::um::handleapi::CloseHandle;
use winapi::um::processthreadsapi::CreateRemoteThread;
use winapi::um::synchapi::WaitForSingleObject;
use winapi::um::processthreadsapi::{CreateProcessA, STARTUPINFOA, PROCESS_INFORMATION};
use winapi::um::winbase::CREATE_NO_WINDOW;
use sha2::{Sha256, Digest};

//Only PrivacyMode.dll allowed to load
const EXPECTED_DLL_HASH: &str = "3163008b75b95adb05bea61e095026beb94ef17ae8ec4e2608bc1eb64c55f83d";

fn get_dll_path() -> Result<PathBuf, String> {
    let exe_path = env::current_exe()
        .map_err(|e| format!("Failed to get executable path: {}", e))?;
    let exe_dir = exe_path.parent().ok_or("Failed to get executable directory")?;
    let dll_path = exe_dir.join("PrivacyMode.dll");
    if !dll_path.exists() {
        return Err(format!("PrivacyMode.dll not found"));
    }
    Ok(dll_path)
}

fn verify_dll_hash(dll_path: &PathBuf) -> Result<(), String> {
    let mut file = File::open(dll_path).map_err(|e| format!("Failed to open DLL: {}", e))?;
    let mut contents = Vec::new();
    file.read_to_end(&mut contents).map_err(|e| format!("Failed to read DLL: {}", e))?;
    let hash = Sha256::digest(&contents);
    let hash_hex = format!("{:x}", hash);
    if EXPECTED_DLL_HASH.is_empty() {
        return Err(format!("Configure hash: {}", hash_hex));
    }
    if hash_hex != EXPECTED_DLL_HASH {
        return Err("DLL hash mismatch".to_string());
    }
    Ok(())
}

fn main() {
    let source = r"C:\Windows\System32\RuntimeBroker.exe";
    
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <app_name>", args[0]);
        return;
    }
    let app_name = &args[1];
    
    let current_path = env::current_dir();
    let destination = match current_path {
        Ok(path) => {
            let destinationlocal = path.join(format!("RuntimeBroker_{}.exe", app_name));
            if destinationlocal.exists() {
                destinationlocal
            } else {
                let tmp_path = env::temp_dir();
                tmp_path.join(format!("RuntimeBroker_{}.exe", app_name))
            }
        }
        Err(_) => {
            let tmp_path = env::temp_dir();
            tmp_path.join(format!("RuntimeBroker_{}.exe", app_name))
        }
    };
    
    let destination2 = destination.clone();
    eprintln!("{:?}", destination2);
    match fs::copy(source, destination) {
        Ok(_) => println!("File copied successfully."),
        Err(e) => println!("Error copying file: {}", e),
    }
    
    let dll_path = match get_dll_path() {
        Ok(path) => path,
        Err(e) => {
            eprintln!("{}", e);
            return;
        }
    };
    
    if let Err(e) = verify_dll_hash(&dll_path) {
        eprintln!("{}", e);
        return;
    }
    
    unsafe {
        if let Err(e) = start_process(&destination2.to_string_lossy()) {
            eprintln!("Error starting RuntimeBroker, error: {}", e);
            return;
        }
    }
    
    let target_pid = unsafe { 
        match find_process_id(&format!("RuntimeBroker_{}.exe", app_name)) {
            Some(pid) => pid,
            None => {
                eprintln!("RuntimeBroker process not found");
                return;
            }
        }
    };
    unsafe {
        inject_dll(target_pid, &dll_path.to_string_lossy()).expect("Failed");
    }
}

unsafe fn find_process_id(process_name: &str) -> Option<DWORD> {
    let mut process_ids = [0u32; 1024];
    let mut needed = 0;

    if EnumProcesses(process_ids.as_mut_ptr(), std::mem::size_of_val(&process_ids) as DWORD, &mut needed) == 0 {
        return None;
    }

    let num_processes = needed as usize / std::mem::size_of::<DWORD>();

    for &pid in process_ids.iter().take(num_processes) {
        let h_process = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid);

        if !h_process.is_null() {
            let mut process_name_buf = [0i8; MAX_PATH];
            if GetProcessImageFileNameA(h_process, process_name_buf.as_mut_ptr(), MAX_PATH as DWORD) != 0 {
                let process_name_osstr = OsString::from_wide(&process_name_buf.iter().map(|&c| c as u16).collect::<Vec<u16>>());
                let process_name_str = process_name_osstr.to_string_lossy().into_owned();
                if process_name_str.contains(process_name) {
                    return Some(pid);
                }
            }
        }
    }

    None
}

unsafe fn inject_dll(target_pid: u32, dll_path: &str) -> Result<(), String> {
    let h_process = OpenProcess(PROCESS_ALL_ACCESS, 0, target_pid);
    if h_process.is_null() {
        return Err(format!("Failed to open target process. Error: {}", GetLastError()));
    }

    let dll_file_utf16: Vec<u16> = dll_path.encode_utf16().chain(Some(0)).collect();

    let buf = VirtualAllocEx(
        h_process,
        null_mut(),
        dll_file_utf16.len() * 2,
        MEM_COMMIT,
        PAGE_READWRITE,
    );
    if buf.is_null() {
        return Err("Failed VirtualAllocEx".to_string());
    }

    let mut written: usize = 0;
    if WriteProcessMemory(
        h_process,
        buf,
        dll_file_utf16.as_ptr() as _,
        dll_file_utf16.len() * 2,
        &mut written,
    ) == 0 {
        return Err(format!("Failed WriteProcessMemory. Error: {}", GetLastError()));
    }

    let kernel32_modulename = CString::new("kernel32.dll").unwrap();
    let hmodule = GetModuleHandleA(kernel32_modulename.as_ptr() as _);
    if hmodule.is_null() {
        return Err("Failed GetModuleHandleA".to_string());
    }

    let load_librarya_name = CString::new("LoadLibraryW").unwrap();
    let load_librarya = GetProcAddress(hmodule, load_librarya_name.as_ptr() as _);
    if load_librarya.is_null() {
        return Err("Failed GetProcAddress of LoadLibraryW".to_string());
    }

    let h_thread = CreateRemoteThread(
        h_process,
        null_mut(),
        0,
        Some(std::mem::transmute(load_librarya)),
        buf as _,
        0,
        null_mut(),
    );
    if h_thread.is_null() {
        return Err(format!("Failed CreateRemoteThread. Error: {}", GetLastError()));
    }

    WaitForSingleObject(h_thread, INFINITE);

    CloseHandle(h_thread);
    CloseHandle(h_process);

    Ok(())
}

unsafe fn start_process(executable_path: &str) -> Result<(), String> {
    let mut startup_info: STARTUPINFOA = std::mem::zeroed();
    let mut process_info: PROCESS_INFORMATION = std::mem::zeroed();

    let success = CreateProcessA(
        CString::new(executable_path).unwrap().as_ptr(),
        null_mut(),
        null_mut(),
        null_mut(),
        0,
        CREATE_NO_WINDOW,
        null_mut(),
        null_mut(),
        &mut startup_info,
        &mut process_info,
    );

    if success == 0 {
        Err(format!("Failed to start process. Error: {}", GetLastError()))
    } else {
        CloseHandle(process_info.hProcess);
        CloseHandle(process_info.hThread);
        Ok(())
    }
}