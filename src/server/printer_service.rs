// Remote Printing Service
// Captures print jobs from virtual printer and sends them to connected clients
//
// Windows: named pipe approach. macOS: CUPS backend + Unix socket.

#[cfg(any(target_os = "windows", target_os = "macos"))]
use hbb_common::{
    log,
    message_proto::*,
};

#[cfg(any(target_os = "windows", target_os = "macos"))]
use std::sync::{
    atomic::{AtomicBool, AtomicI32, Ordering},
    mpsc, Mutex,
};

pub const NAME: &'static str = "remote-printer";

#[cfg(any(target_os = "windows", target_os = "macos"))]
lazy_static::lazy_static! {
    static ref PRINT_SERVICE_RUNNING: AtomicBool = AtomicBool::new(false);
    static ref NEXT_JOB_ID: AtomicI32 = AtomicI32::new(1);
    static ref PIPE_STOP_TX: Mutex<Option<mpsc::Sender<()>>> = Mutex::new(None);
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
#[derive(Clone)]
pub struct PrintJob {
    pub id: i32,
    pub data: Vec<u8>,
}

/// Start remote printing service
/// Called when user enables "Enable Remote Printing" option
#[cfg(any(target_os = "windows", target_os = "macos"))]
pub fn start_remote_printing() {
    use crate::platform::{start_print_pipe_server, VIRTUAL_PRINTER_NAME};

    log::info!("start_remote_printing called, pid={}", std::process::id());

    if PRINT_SERVICE_RUNNING.load(Ordering::SeqCst) {
        log::info!("Print service already running");
        return;
    }

    // Start the named pipe server
    // When a print job is captured, call on_printer_data to send to connections
    match start_print_pipe_server(move |data| {
        let job_id = NEXT_JOB_ID.fetch_add(1, Ordering::SeqCst);
        log::info!("Print job {} received from pipe: {} bytes", job_id, data.len());

        // Send print data to active connections via on_printer_data
        crate::server::on_printer_data(data);
    }) {
        Ok(stop_handle) => {
            {
                let mut stop_tx = PIPE_STOP_TX.lock().unwrap();
                *stop_tx = Some(stop_handle);
            }
            PRINT_SERVICE_RUNNING.store(true, Ordering::SeqCst);
            log::info!("Remote printing service started. Virtual printer: {}", VIRTUAL_PRINTER_NAME);
        }
        Err(e) => {
            log::error!("Failed to start print pipe server: {}", e);
        }
    }
}

/// Stop remote printing service
/// Called when user disables "Enable Remote Printing" option
#[cfg(any(target_os = "windows", target_os = "macos"))]
pub fn stop_remote_printing() {
    if !PRINT_SERVICE_RUNNING.load(Ordering::SeqCst) {
        return;
    }

    PRINT_SERVICE_RUNNING.store(false, Ordering::SeqCst);

    // Signal the pipe server to stop
    if let Some(stop_tx) = PIPE_STOP_TX.lock().unwrap().take() {
        let _ = stop_tx.send(());
    }

    // On Windows: connect briefly to the pipe to unblock ConnectNamedPipe.
    #[cfg(target_os = "windows")]
    {
        use crate::platform::PRINTER_PIPE_NAME;
        std::thread::spawn(|| {
            use std::fs::OpenOptions;
            let _ = OpenOptions::new().write(true).open(PRINTER_PIPE_NAME);
        });
    }
    // On macOS: the Unix socket server uses accept timeouts, so the stop signal
    // will be picked up within a few seconds without needing to unblock.

    log::info!("Remote printing service stopped");
}

/// Check if print service is running
#[cfg(any(target_os = "windows", target_os = "macos"))]
pub fn is_print_service_running() -> bool {
    PRINT_SERVICE_RUNNING.load(Ordering::SeqCst)
}

/// Create messages to send a print job to the peer
/// Returns a Vec of messages: first the request, then data blocks, then done
#[cfg(any(target_os = "windows", target_os = "macos"))]
pub fn create_print_job_messages(job: &PrintJob) -> Vec<Message> {
    let mut messages = Vec::new();

    // First send the file transfer request
    let mut send_req = FileTransferSendRequest::new();
    send_req.id = job.id;
    send_req.path = format!("print_job_{}.pdf", job.id);
    send_req.include_hidden = false;
    send_req.file_num = 0;
    send_req.file_type = file_transfer_send_request::FileType::Printer.into();

    let mut file_action = FileAction::new();
    file_action.set_send(send_req);

    let mut msg = Message::new();
    msg.set_file_action(file_action);
    messages.push(msg);

    // Then send the data blocks
    const BLOCK_SIZE: usize = 65536; // 64KB blocks

    for (blk_id, chunk) in job.data.chunks(BLOCK_SIZE).enumerate() {
        let mut block = FileTransferBlock::new();
        block.id = job.id;
        block.file_num = 0;
        block.data = chunk.to_vec().into();
        block.compressed = false;
        block.blk_id = blk_id as u32;

        let mut msg = Message::new();
        msg.set_file_response(FileResponse {
            union: Some(file_response::Union::Block(block)),
            ..Default::default()
        });
        messages.push(msg);
    }

    // Finally send done message
    let mut done = FileTransferDone::new();
    done.id = job.id;
    done.file_num = 0;

    let mut msg = Message::new();
    msg.set_file_response(FileResponse {
        union: Some(file_response::Union::Done(done)),
        ..Default::default()
    });
    messages.push(msg);

    messages
}

// Stub implementations for platforms without printing support (Linux, etc.)
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn start_remote_printing() {
    hbb_common::log::warn!("Remote printing is not available on this platform");
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn stop_remote_printing() {}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn is_print_service_running() -> bool {
    false
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub struct PrintJob {
    pub id: i32,
    pub data: Vec<u8>,
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn create_print_job_messages(_job: &PrintJob) -> Vec<hbb_common::message_proto::Message> {
    Vec::new()
}
