use std::os::unix::fs::{PermissionsExt as _, symlink};

use super::*;

const PASSWORD: &str = "c2VjcmV0LXBhc3N3b3JkIQ==";

/// Splits concatenated `OSC 5522 ; metadata [; payload] ST` packets.
fn packets(reply: &str) -> Vec<(String, String)> {
    reply
        .split_terminator("\x1b\\")
        .map(|packet| {
            let body = packet
                .strip_prefix("\x1b]5522;")
                .expect("packet starts with OSC 5522");
            let (metadata, payload) = body.split_once(';').unwrap_or((body, ""));
            (metadata.to_owned(), payload.to_owned())
        })
        .collect()
}

fn offered(png: &[u8], text: Option<&str>) -> (PasteEvents, Instant) {
    let events = PasteEvents::default();
    let now = Instant::now();
    events.announce(
        png.to_vec(),
        text.map(str::to_owned),
        PASSWORD.to_owned(),
        now,
    );
    (events, now)
}

fn read(events: &PasteEvents, body: &str, now: Instant) -> Vec<(String, String)> {
    packets(
        &events
            .reply(&TerminalRequest::ClipboardPacket(body.to_owned()), now)
            .expect("read requests are answered"),
    )
}

fn decoded_data(packets: &[(String, String)], mime: &str) -> Vec<u8> {
    let metadata = format!("type=read:status=DATA:mime={}", BASE64.encode(mime));
    packets
        .iter()
        .filter(|(candidate, _)| *candidate == metadata)
        .flat_map(|(_, payload)| BASE64.decode(payload).unwrap())
        .collect()
}

#[test]
fn announcement_lists_each_type_under_a_one_time_password() {
    let events = PasteEvents::default();
    let announcement = events.announce(
        b"png".to_vec(),
        Some("caption".to_owned()),
        PASSWORD.to_owned(),
        Instant::now(),
    );
    assert_eq!(
        packets(&announcement),
        vec![
            (format!("type=read:status=OK:pw={PASSWORD}"), String::new()),
            (
                "type=read:status=DATA:mime=aW1hZ2UvcG5n".to_owned(),
                String::new()
            ),
            (
                "type=read:status=DATA:mime=dGV4dC9wbGFpbg==".to_owned(),
                String::new()
            ),
            ("type=read:status=DONE".to_owned(), String::new()),
        ]
    );
    let image_only = events.announce(b"png".to_vec(), None, PASSWORD.to_owned(), Instant::now());
    assert_eq!(packets(&image_only).len(), 3);
}

#[test]
fn mime_key_read_with_the_password_is_served_once_in_padded_chunks() {
    let image = (0..10_001_usize)
        .map(|value| value.to_le_bytes()[0])
        .collect::<Vec<_>>();
    let (events, now) = offered(&image, None);
    let request = format!("type=read:pw={PASSWORD}:name=UGFzdGUgZXZlbnQ=:mime=aW1hZ2UvcG5n");

    let reply = read(&events, &request, now);
    assert_eq!(reply.first().unwrap().0, "type=read:status=OK");
    assert_eq!(reply.last().unwrap().0, "type=read:status=DONE");
    let chunks = &reply[1..reply.len() - 1];
    assert_eq!(chunks.len(), 3);
    for (_, payload) in chunks {
        assert_eq!(payload.len() % 4, 0, "every chunk is independently padded");
        assert!(BASE64.decode(payload).unwrap().len() <= MAX_DATA_CHUNK_BYTES);
    }
    assert_eq!(decoded_data(&reply, PNG_MIME), image);

    assert_eq!(
        read(&events, &request, now),
        vec![("type=read:status=EPERM".to_owned(), String::new())],
        "a paste is single use"
    );
}

#[test]
fn payload_list_reads_serve_every_available_requested_type_in_order() {
    let (events, now) = offered(b"image-bytes", Some("caption"));
    let payload = BASE64.encode("application/pdf text/plain image/png");
    let reply = read(&events, &format!("type=read:pw={PASSWORD};{payload}"), now);

    let mimes = reply
        .iter()
        .filter_map(|(metadata, _)| metadata.strip_prefix("type=read:status=DATA:mime="))
        .map(|mime| String::from_utf8(BASE64.decode(mime).unwrap()).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(mimes, ["text/plain", "image/png"]);
    assert_eq!(decoded_data(&reply, TEXT_MIME), b"caption");
    assert_eq!(decoded_data(&reply, PNG_MIME), b"image-bytes");
}

#[test]
fn kitty_listing_reads_report_types_without_consuming_the_paste() {
    let (events, now) = offered(b"png", Some("caption"));
    let listing = read(
        &events,
        &format!("type=read:pw={PASSWORD};{}", BASE64.encode(".")),
        now,
    );
    assert_eq!(listing[1].0, "type=read:status=DATA:mime=Lg==");
    assert_eq!(
        BASE64.decode(&listing[1].1).unwrap(),
        b"image/png text/plain"
    );
    let reply = read(
        &events,
        &format!("type=read:pw={PASSWORD}:mime=aW1hZ2UvcG5n"),
        now,
    );
    assert_eq!(decoded_data(&reply, PNG_MIME), b"png");
}

#[test]
fn wrong_password_unavailable_type_and_expiry_are_refused() {
    let (events, now) = offered(b"png", None);
    let eperm = vec![("type=read:status=EPERM".to_owned(), String::new())];
    assert_eq!(
        read(&events, "type=read:pw=d3Jvbmc=:mime=aW1hZ2UvcG5n", now),
        eperm
    );
    assert_eq!(read(&events, "type=read:mime=aW1hZ2UvcG5n", now), eperm);
    assert_eq!(
        read(
            &events,
            &format!(
                "type=read:pw={PASSWORD}:mime={}",
                BASE64.encode("text/plain")
            ),
            now,
        ),
        eperm
    );
    // A refused read does not consume the pending paste.
    let later = now + PENDING_PASTE_TTL;
    let request = format!("type=read:pw={PASSWORD}:mime=aW1hZ2UvcG5n");
    assert_eq!(
        decoded_data(&read(&events, &request, later), PNG_MIME),
        b"png"
    );

    let (events, now) = offered(b"png", None);
    let expired = now + PENDING_PASTE_TTL + Duration::from_secs(1);
    assert_eq!(read(&events, &request, expired), eperm);
}

#[test]
fn a_new_paste_replaces_the_pending_one() {
    let (events, now) = offered(b"first", None);
    events.announce(b"second".to_vec(), None, "bmV3".to_owned(), now);
    let old = format!("type=read:pw={PASSWORD}:mime=aW1hZ2UvcG5n");
    assert_eq!(read(&events, &old, now)[0].0, "type=read:status=EPERM");
    let reply = read(&events, "type=read:pw=bmV3:mime=aW1hZ2UvcG5n", now);
    assert_eq!(decoded_data(&reply, PNG_MIME), b"second");
}

#[test]
fn primary_selection_malformed_reads_and_ids_are_handled() {
    let (events, now) = offered(b"png", None);
    assert_eq!(
        read(&events, "type=read:loc=primary:id=a/b:c;Lg==", now),
        vec![("type=read:status=ENOSYS:id=ab".to_owned(), String::new())]
    );
    assert_eq!(
        read(&events, "type=read:pw=x:id=x$1;%%%", now)[0].0,
        "type=read:status=EINVAL:id=x1"
    );
    let reply = read(
        &events,
        &format!("type=read:pw={PASSWORD}:id=req-1.a+b_c:mime=aW1hZ2UvcG5n"),
        now,
    );
    assert!(
        reply
            .iter()
            .all(|(metadata, _)| metadata.ends_with(":id=req-1.a+b_c"))
    );
    // Writes, echoes of terminal packets, and other OSC 5522 types are ignored.
    for ignored in [
        "type=write:mime=eA==",
        "type=read:status=OK",
        "type=wdata;eA==",
    ] {
        assert!(
            events
                .reply(&TerminalRequest::ClipboardPacket(ignored.to_owned()), now)
                .is_none()
        );
    }
}

#[test]
fn decrqm_reports_set_or_reset() {
    let events = PasteEvents::default();
    let now = Instant::now();
    assert_eq!(
        events.reply(
            &TerminalRequest::ReportEnhancedPasteMode { enabled: true },
            now
        ),
        Some("\x1b[?5522;1$y".to_owned())
    );
    assert_eq!(
        events.reply(
            &TerminalRequest::ReportEnhancedPasteMode { enabled: false },
            now
        ),
        Some("\x1b[?5522;2$y".to_owned())
    );
}

fn private_paste_directory() -> (tempdir::TempDir, PathBuf) {
    let root = tempdir::TempDir::new();
    let directory = root.path().join(PASTE_DIRECTORY_NAME);
    std::fs::create_dir(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    (root, directory)
}

fn png_bytes(extra: usize) -> Vec<u8> {
    let mut bytes = PNG_SIGNATURE.to_vec();
    bytes.resize(PNG_SIGNATURE.len() + extra, 7);
    bytes
}

#[test]
fn paste_image_reads_and_deletes_a_png_in_the_private_directory() {
    let (_root, directory) = private_paste_directory();
    let path = directory.join("clipboard-1.png");
    std::fs::write(&path, png_bytes(32)).unwrap();

    assert_eq!(take_paste_image(&path, &directory).unwrap(), png_bytes(32));
    assert!(!path.exists());
}

#[test]
fn paste_image_rejects_paths_outside_the_directory_symlinks_non_png_and_oversize() {
    let (root, directory) = private_paste_directory();
    let outside = root.path().join("outside.png");
    std::fs::write(&outside, png_bytes(8)).unwrap();
    assert!(take_paste_image(&outside, &directory).is_err());
    let traversal = directory.join("..").join("outside.png");
    assert!(take_paste_image(&traversal, &directory).is_err());
    assert!(take_paste_image(Path::new("clipboard.png"), Path::new("")).is_err());
    assert!(outside.exists());

    let link = directory.join("link.png");
    symlink(&outside, &link).unwrap();
    let error = take_paste_image(&link, &directory).unwrap_err();
    assert!(
        format!("{error:#}").contains("open pasted image"),
        "{error:#}"
    );
    assert!(link.exists() && outside.exists());

    let text = directory.join("clipboard-2.png");
    std::fs::write(&text, b"not a png").unwrap();
    let error = take_paste_image(&text, &directory).unwrap_err();
    assert!(error.to_string().contains("not a PNG"));
    assert!(text.exists(), "rejected files are left in place");

    let oversized = directory.join("clipboard-3.png");
    let file = std::fs::File::create(&oversized).unwrap();
    file.set_len(MAX_PASTE_IMAGE_BYTES + 1).unwrap();
    let error = take_paste_image(&oversized, &directory).unwrap_err();
    assert!(error.to_string().contains("25 MiB"));

    let subdirectory = directory.join("nested.png");
    std::fs::create_dir(&subdirectory).unwrap();
    assert!(take_paste_image(&subdirectory, &directory).is_err());

    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o777)).unwrap();
    let valid = directory.join("clipboard-4.png");
    std::fs::write(&valid, png_bytes(1)).unwrap();
    let error = take_paste_image(&valid, &directory).unwrap_err();
    assert!(error.to_string().contains("private directory"));
}

/// A minimal kitty paste-event client in the pane, modeled on omp's: it
/// enables mode 5522, checks DECRQM, waits for an announcement, reads
/// `image/png` with the one-time password, and saves the decoded bytes.
const PASTE_CLIENT: &str = r#"
import base64, os, re, sys, tty
tty.setraw(0)
os.write(1, b"\x1b[?5522h\x1b[?5522$p")
buf = b""
def read_until(marker):
    global buf
    while marker not in buf:
        data = os.read(0, 1 << 20)
        if not data:
            sys.exit(1)
        buf += data
read_until(b"\x1b[?5522;1$y")
read_until(b"status=DONE")
pw = re.search(rb"status=OK:pw=([A-Za-z0-9+/=]+)", buf).group(1)
buf = b""
os.write(1, b"\x1b]5522;type=read:pw=" + pw + b":name=UGFzdGUgZXZlbnQ=:mime=aW1hZ2UvcG5n\x07")
read_until(b"status=DONE")
chunks = re.findall(rb"status=DATA:mime=aW1hZ2UvcG5n;([A-Za-z0-9+/=]*)\x1b\\", buf)
with open(sys.argv[1] + ".tmp", "wb") as out:
    out.write(b"".join(base64.b64decode(chunk) for chunk in chunks))
os.rename(sys.argv[1] + ".tmp", sys.argv[1])
"#;

fn python3() -> Option<PathBuf> {
    [
        "/opt/homebrew/bin/python3",
        "/usr/local/bin/python3",
        "/usr/bin/python3",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|path| {
        std::process::Command::new(path)
            .args(["-c", "import base64, re, tty"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

fn large_png() -> Vec<u8> {
    let mut png = PNG_SIGNATURE.to_vec();
    let mut state = 0x2545_f491_u32;
    png.extend((0..3 * 1024 * 1024).map(|_| {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        state.to_le_bytes()[0]
    }));
    png
}

fn assert_paste_round_trip(session: &Arc<PtySession>, output: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !session.enhanced_paste() {
        assert!(Instant::now() < deadline, "the client never enabled 5522");
        thread::sleep(Duration::from_millis(20));
    }
    let png = large_png();
    session.paste_image(png.clone(), None).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !output.exists() {
        assert!(
            Instant::now() < deadline,
            "the client never received the paste"
        );
        thread::sleep(Duration::from_millis(20));
    }
    assert!(std::fs::read(output).unwrap() == png, "pasted bytes differ");
}

#[test]
fn a_pty_pane_client_reads_a_large_paste_through_the_pty() {
    let Some(python) = python3() else {
        return;
    };
    let root = tempdir::TempDir::new();
    let script = root.path().join("client.py");
    std::fs::write(&script, PASTE_CLIENT).unwrap();
    let output = root.path().join("pasted.png");
    let mut command = portable_pty::CommandBuilder::new(python);
    command.arg(&script);
    command.arg(&output);
    let session = PtySession::spawn_command(Uuid::new_v4(), command, "paste client").unwrap();
    assert_paste_round_trip(&session, &output);
    session.terminate_and_wait().unwrap();
}

#[test]
fn a_managed_tmux_pane_client_reads_a_large_paste_through_a_paste_buffer() {
    use std::collections::HashMap;

    use crate::tmux::system_tmux_binary;
    use crate::tmux_control::{PaneSinks, PrivateTmuxServerGuard, TmuxControlClient, TmuxServer};

    let (Ok(binary), Some(python)) = (system_tmux_binary(), python3()) else {
        return;
    };
    let root = tempdir::TempDir::new();
    let config_path = root.path().join("hh.conf");
    std::fs::write(
        &config_path,
        concat!(
            include_str!("../bundled/hh.tmux.conf"),
            "set -g default-shell '/bin/sh'\n"
        ),
    )
    .unwrap();
    let token = Uuid::new_v4().simple().to_string();
    let socket_name = format!("hh-test-{}", &token[..12]);
    let _server_guard = PrivateTmuxServerGuard {
        binary: binary.clone(),
        socket_name: socket_name.clone(),
    };
    let server = TmuxServer {
        binary,
        socket_name,
        config_path,
    };
    let sinks: PaneSinks = Arc::new(Mutex::new(HashMap::new()));
    let client = TmuxControlClient::spawn(&server, &format!("hh-{}", &token[..12]), sinks).unwrap();
    let session =
        PtySession::spawn_tmux(Uuid::new_v4(), Uuid::new_v4(), None, root.path(), &client).unwrap();
    let script = root.path().join("client.py");
    std::fs::write(&script, PASTE_CLIENT).unwrap();
    let output = root.path().join("pasted.png");
    let command = format!(
        "'{}' '{}' '{}'\r",
        python.display(),
        script.display(),
        output.display()
    );
    session.write_input(command.as_bytes()).unwrap();
    assert_paste_round_trip(&session, &output);
    session.terminate_and_wait().unwrap();
    client.kill_session().unwrap();
}

mod tempdir {
    use std::path::{Path, PathBuf};

    /// A private scratch directory removed on drop.
    pub(super) struct TempDir(PathBuf);

    impl TempDir {
        pub(super) fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("hh-paste-events-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        pub(super) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
