use std::{
    ffi::OsStr,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::Duration,
};

use cloud_filter::{
    error::{CResult, CloudErrorKind},
    filter::{info, ticket, Request, SyncFilter},
    placeholder_file::PlaceholderFile,
    root::{
        HydrationType, PopulationType, Session, SupportedAttribute, SyncRootIdBuilder, SyncRootInfo,
    },
    utility::WriteAt,
};
use wfd::DialogParams;

// MUST be a multiple of 4096
const CHUNK_SIZE_BYTES: usize = 4096;
const CHUNK_DELAY_MS: u64 = 250;

const SERVER_PATH: Option<&str> = Some("C:\\Users\\nicky\\Music\\server");
const CLIENT_PATH: Option<&str> = Some("C:\\Users\\nicky\\Music\\client");

const PROVIDER_NAME: &str = "TestStorageProvider";
const ACCOUNT_NAME: &str = "TestAccount1";
const DISPLAY_NAME: &str = "TestStorageProviderDisplayName";
const VERSION: &str = "1.0.0";

fn main() {
    let server_path = SERVER_PATH
        .map(PathBuf::from)
        .filter(|path| path.exists())
        .unwrap_or_else(|| {
            wfd::open_dialog(DialogParams {
                file_name_label: "Server Folder",
                title: "Select Server Directory",
                options: wfd::FOS_PICKFOLDERS,
                ..Default::default()
            })
            .unwrap()
            .selected_file_path
        });

    let client_path = CLIENT_PATH
        .map(PathBuf::from)
        .filter(|path| path.exists())
        .unwrap_or_else(|| {
            wfd::open_dialog(DialogParams {
                file_name_label: "Client Folder",
                title: "Select Client Directory",
                options: wfd::FOS_PICKFOLDERS,
                ..Default::default()
            })
            .unwrap()
            .selected_file_path
        });

    let sync_root_id = SyncRootIdBuilder::new(PROVIDER_NAME)
        .account_name(ACCOUNT_NAME)
        .build();

    // impl COM objects

    if !sync_root_id.is_registered().unwrap() {
        sync_root_id
            .register(
                SyncRootInfo::default()
                    .with_display_name(DISPLAY_NAME)
                    .with_hydration_type(HydrationType::Full)
                    .with_population_type(PopulationType::AlwaysFull)
                    .with_icon("%SystemRoot%\\system32\\charmap.exe,0")
                    .with_version(VERSION)
                    .with_path(&client_path)
                    .unwrap()
                    .with_allow_hardlinks(false)
                    .with_show_siblings_as_group(false)
                    .with_supported_attribute(
                        SupportedAttribute::FileCreationTime
                            | SupportedAttribute::DirectoryCreationTime,
                    ),
            )
            .unwrap();
    }

    let connection = Session::new()
        .connect(
            //     .require_process_info(true) << FIXME THIS IS LOST
            &client_path,
            Filter {
                client_path: client_path.clone(),
                server_path: server_path.clone(),
            },
        )
        .unwrap();

    create_placeholders(&server_path, Path::new(""), &client_path);

    // TODO: hydrate and dehydrate on pin/unpin

    // wait until a key is pressed to exit
    let (tx, rx) = mpsc::channel();
    ctrlc::set_handler(move || tx.send(()).unwrap()).unwrap();
    rx.recv().unwrap();

    drop(connection);

    sync_root_id.unregister().unwrap();

    // cleanup any placeholders whilst keeping the client folder intact
    std::fs::read_dir(&client_path).unwrap().for_each(|entry| {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            std::fs::remove_dir_all(entry.path()).unwrap()
        } else {
            std::fs::remove_file(entry.path()).unwrap()
        }
    });
}

fn create_placeholders(source_path: &Path, source_subdir: &Path, dest_path: &Path) {
    for entry in std::fs::read_dir(source_path.join(source_subdir))
        .unwrap()
        .flatten()
    {
        let source_file_path = entry.path();
        let metadata = entry.metadata().unwrap();
        let is_dir = metadata.is_dir();
        let file_name = entry.file_name();
        let relative_path = source_subdir.join(&file_name);

        println!("Found {source_file_path:?}, is_dir: {is_dir}");

        let blob = relative_path.clone().into_os_string().into_encoded_bytes();
        PlaceholderFile::new(&file_name)
            .metadata(metadata.into())
            .mark_in_sync()
            .blob(blob)
            .has_no_children()
            .overwrite()
            .create::<PathBuf>(dest_path.join(source_subdir))
            .unwrap();

        if is_dir {
            create_placeholders(source_path, &relative_path, dest_path);
        }

        // TODO: apply custom state to placeholder like in sample
    }
}

#[derive(Debug)]
struct Filter {
    client_path: PathBuf,
    server_path: PathBuf,
}

impl Filter {
    fn client_to_server_path(&self, full_path: &Path) -> PathBuf {
        let relative_path = full_path.strip_prefix(&self.client_path).unwrap();
        self.server_path.join(relative_path)
    }
}

impl SyncFilter for Filter {
    fn fetch_data(
        &self,
        request: Request,
        ticket: ticket::FetchData,
        info: info::FetchData,
    ) -> CResult<()> {
        let request_path = request.path();
        println!("fetch_data, path: {request_path:?}, info {info:?}");

        let relative_path =
            Path::new(unsafe { OsStr::from_encoded_bytes_unchecked(request.file_blob()) });

        let source_file_path = self.server_path.join(relative_path);

        // due to `PopulationPolicy::AlwaysFull`, this will always be the range of the
        // entire file (I think)
        let range = info.required_file_range();
        let mut source_file = File::open(source_file_path).unwrap();
        source_file.seek(SeekFrom::Start(range.start)).unwrap();

        // reuse the buffer to avoid allocations
        let mut buffer = [0; CHUNK_SIZE_BYTES];

        // TODO: if anything in here fails then just keep retrying like in the sample
        // TODO: create a less naive impl
        let total = range.end - range.start;
        let mut position = range.start;
        loop {
            // set the progress (transfer dialog + progress bar) in the beginning of the
            // loop to account for 0 progress and to make it seem more responsive
            let completed = position - range.start;
            ticket.report_progress(total, completed).unwrap();

            // TODO: read directly to the BufWriters buffer
            // TODO: ignore interrupted errors
            let mut bytes_read = source_file.read(&mut buffer).unwrap();

            let unaligned = bytes_read % 4096;
            if unaligned != 0 && position + (bytes_read as u64) < range.end {
                bytes_read -= unaligned;
                source_file
                    .seek(SeekFrom::Current(-(unaligned as i64)))
                    .unwrap();
            }
            ticket.write_at(&buffer[0..bytes_read], position).unwrap();
            position += bytes_read as u64;

            // if everything is downloaded then we're done
            if position >= range.end {
                break;
            }

            // simulate network latency
            thread::sleep(Duration::from_millis(CHUNK_DELAY_MS))
        }

        // TODO: if anything fails (remove unwraps) then call TransferData with
        // a failure CompletionStatus

        Ok(())
    }

    fn validate_data(
        &self,
        request: Request,
        _ticket: ticket::ValidateData,
        info: info::ValidateData,
    ) -> CResult<()> {
        let request_path = request.path();
        println!("validate_data, request_path: {request_path:?}, info: {info:?}");
        Ok(())
    }

    fn cancel_fetch_data(&self, request: Request, info: info::CancelFetchData) {
        let request_path = request.path();
        println!("cancel_fetch_data, request_path: {request_path:?}, info: {info:?}");
    }

    fn fetch_placeholders(
        &self,
        request: Request,
        _ticket: ticket::FetchPlaceholders,
        info: info::FetchPlaceholders,
    ) -> CResult<()> {
        let request_path = request.path();
        println!("fetch_placeholders, request_path: {request_path:?}, info: {info:?}");
        // This won't be called because we use PopulationType::AlwaysFull
        Err(CloudErrorKind::NotSupported)
    }

    fn cancel_fetch_placeholders(&self, request: Request, info: info::CancelFetchPlaceholders) {
        let request_path = request.path();
        println!("cancel_fetch_placeholders, request_path: {request_path:?}, info: {info:?}");
    }

    fn opened(&self, request: Request, info: info::Opened) {
        let request_path = request.path();
        println!("opened, request_path: {request_path:?}, info: {info:?}");
    }

    fn closed(&self, request: Request, info: info::Closed) {
        let request_path = request.path();
        println!("closed, request_path: {request_path:?}, info: {info:?}");
    }

    fn dehydrate(
        &self,
        request: Request,
        _ticket: ticket::Dehydrate,
        info: info::Dehydrate,
    ) -> CResult<()> {
        let request_path = request.path();
        println!("dehydrate, request_path: {request_path:?}, info: {info:?}");
        Err(CloudErrorKind::NotSupported)
    }

    fn dehydrated(&self, request: Request, info: info::Dehydrated) {
        let request_path = request.path();
        println!("dehydrated, request_path: {request_path:?}, info: {info:?}");
    }

    fn delete(&self, request: Request, ticket: ticket::Delete, info: info::Delete) -> CResult<()> {
        let request_path = request.path();
        println!("delete, request_path: {request_path:?}, info: {info:?}");

        if info.is_undelete() {
            Err(CloudErrorKind::NotSupported)?;
        }

        let delete_path = self.client_to_server_path(&request_path);
        if info.is_directory() {
            std::fs::remove_dir(delete_path).map_err(|_| CloudErrorKind::Unsuccessful)?;
        } else {
            std::fs::remove_file(delete_path).map_err(|_| CloudErrorKind::Unsuccessful)?;
        }

        ticket.pass().unwrap();

        Ok(())
    }

    fn deleted(&self, request: Request, info: info::Deleted) {
        let request_path = request.path();
        println!("deleted, request_path: {request_path:?}, info: {info:?}");
    }

    fn rename(&self, request: Request, ticket: ticket::Rename, info: info::Rename) -> CResult<()> {
        let request_path = request.path();
        println!("rename, request_path: {request_path:?}, info: {info:?}");

        let target_path = info.target_path();

        match (info.source_in_scope(), info.target_in_scope()) {
            (true, true) => {
                std::fs::rename(
                    self.client_to_server_path(&request_path),
                    self.client_to_server_path(&target_path),
                )
                .map_err(|_| CloudErrorKind::Unsuccessful)?;
            }
            (true, false) => {}
            (false, true) => Err(CloudErrorKind::NotSupported)?, // TODO
            (false, false) => Err(CloudErrorKind::InvalidRequest)?,
        }

        ticket.pass().unwrap();

        Ok(())
    }

    fn renamed(&self, request: Request, info: info::Renamed) {
        let request_path = request.path();
        println!("renamed, request_path: {request_path:?}, info: {info:?}");
    }
}
