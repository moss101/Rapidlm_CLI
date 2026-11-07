//! `rapid acp` end to end: the real binary serving an editor the test plays
//! over the process's stdio, with a scripted model on loopback.
//!
//! What the unit tests cannot show: that a prompt survives every approval
//! its turn raises — the editor's answers route to the waiting prompt, the
//! serve stays up, the prompt ends with its real stop reason, and a cancel
//! during an approval wait ends it — that an answer is read in the
//! protocol's shape, so an approved write lands in the project and a
//! rejected one does not, that "Allow always" is offered only where it is
//! kept and, chosen, stops the same call being asked again, and that each
//! prompt streams only its own turn's events, never the previous turn's
//! again.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Generous: a scripted turn finishes in well under a second; the bound only
/// keeps a regression from hanging the suite.
const DEADLINE: Duration = Duration::from_secs(120);

fn config_doc(base_url: &str) -> String {
    format!(
        "[models]\ndefault = \"local\"\n\
         \n\
         [model.local]\n\
         provider = \"openai-compatible\"\n\
         model = \"test-model\"\n\
         base_url = \"{base_url}\"\n\
         api_key = \"scripted-key\"\n"
    )
}

/// A non-streaming chat completion proposing one `workspace_write`.
fn write_call(id: &str, path: &str) -> String {
    write_calls(&[(id, path)])
}

/// A non-streaming chat completion proposing one `workspace_write` per
/// `(id, path)`, all in one model step.
fn write_calls(calls: &[(&str, &str)]) -> String {
    let tool_calls: Vec<Value> = calls
        .iter()
        .map(|(id, path)| {
            json!({
                "id": id,
                "type": "function",
                "function": {
                    "name": "workspace_write",
                    "arguments": json!({ "path": path, "content": "from the model" }).to_string(),
                },
            })
        })
        .collect();
    json!({
        "choices": [{
            "message": { "role": "assistant", "content": null, "tool_calls": tool_calls },
            "finish_reason": "tool_calls",
        }],
        "usage": { "prompt_tokens": 3, "completion_tokens": 5 },
    })
    .to_string()
}

/// A non-streaming chat completion proposing one call of `tool`.
fn tool_call(id: &str, tool: &str, arguments: Value) -> String {
    json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": id,
                    "type": "function",
                    "function": { "name": tool, "arguments": arguments.to_string() },
                }],
            },
            "finish_reason": "tool_calls",
        }],
        "usage": { "prompt_tokens": 3, "completion_tokens": 5 },
    })
    .to_string()
}

/// A `sleep` duration for test `test` that no other process uses — not
/// another test here, not another run — so its command is found by argv.
#[cfg(unix)]
fn sleep_marker(test: u32) -> String {
    format!("3{test}.{}", std::process::id())
}

/// Whether a `sleep <seconds>` process is running.
#[cfg(unix)]
fn sleeping(seconds: &str) -> bool {
    Command::new("pgrep")
        .args(["-f", &format!("sleep {seconds}")])
        .output()
        .is_ok_and(|out| out.status.success())
}

/// A terminal answer.
fn answer(text: &str) -> String {
    json!({
        "choices": [{
            "message": { "role": "assistant", "content": text },
            "finish_reason": "stop",
        }],
        "usage": { "prompt_tokens": 3, "completion_tokens": 2 },
    })
    .to_string()
}

#[test]
fn headless_large_video_uploads_uses_and_deletes_named_file() {
    let home = temp_dir("large-video");
    let (project, config) = trusted_project(&home.0, |_| answer("unused"));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        let mut name = String::new();
        for step in 0..6 {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(20)))
                .unwrap();
            let request = read_request(&mut stream);
            let (extra, body) = match step {
                0 => {
                    let start: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
                    name = start["file"]["name"].as_str().unwrap().to_owned();
                    (format!("X-Goog-Upload-URL: http://{addr}/upload/v1beta/files?upload_id=fixture\r\n"), String::new())
                }
                1 => (String::new(), json!({"file":{"name":name,"mimeType":"video/mp4"}}).to_string()),
                2 | 3 => (String::new(), json!({"name":name,"mimeType":"video/mp4","state":"ACTIVE","uri":format!("http://{addr}/media/{name}")}).to_string()),
                4 => (String::new(), json!({"candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"large video received"}]},"finishReason":"STOP"}]}).to_string()),
                _ => (String::new(), String::new()),
            };
            requests.push(request);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        }
        (name, requests)
    });
    let config_text =
        config_doc(&format!("http://{addr}/v1beta")).replace("openai-compatible", "google-genai");
    std::fs::write(&config, format!("{config_text}video_input = true\n")).unwrap();
    let mut video = b"\x00\x00\x00\x10ftypisomtest".to_vec();
    video.resize(llm_router::provider::MAX_INLINE_VIDEO_BYTES + 1, 0);
    std::fs::write(project.join("clip.mp4"), &video).unwrap();
    let result = rapid(&project, &home.0, &config)
        .args(["exec", "Describe @clip.mp4"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    let (name, requests) = server.join().unwrap();
    assert!(requests[1].contains("x-goog-upload-command: upload, finalize\r\n"));
    assert_eq!(
        requests[1].split_once("\r\n\r\n").unwrap().1.as_bytes(),
        video
    );
    let inference: Value =
        serde_json::from_str(requests[4].split_once("\r\n\r\n").unwrap().1).unwrap();
    let parts = inference["contents"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|item| item["parts"].as_array().unwrap())
        .collect::<Vec<_>>();
    assert!(
        parts
            .iter()
            .any(|part| part["fileData"]["fileUri"] == format!("http://{addr}/media/{name}"))
    );
    assert!(requests[5].starts_with(&format!("DELETE /v1beta/{name} HTTP/1.1\r\n")));
    let journal = home.0.join(".rapidlm/video-uploads");
    let retained: Vec<_> = std::fs::read_dir(journal)
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            (path.extension().is_some_and(|ext| ext == "json"))
                .then(|| std::fs::read_to_string(path).unwrap())
        })
        .collect();
    assert!(
        retained.is_empty(),
        "retained={retained:?}, stdout={}, stderr={}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn headless_large_video_missing_provider_uri_fails_and_deletes_upload() {
    for missing_after_ready in [false, true] {
        let home = temp_dir("large-video-missing-uri");
        let (project, config) = trusted_project(&home.0, |_| answer("unused"));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut name = String::new();
            let mut requests = Vec::new();
            for step in 0..if missing_after_ready { 5 } else { 4 } {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(20)))
                    .unwrap();
                let request = read_request(&mut stream);
                let (extra, body) = match step {
                0 => {
                    let start: Value =
                        serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
                    name = start["file"]["name"].as_str().unwrap().to_owned();
                    (
                        format!(
                            "X-Goog-Upload-URL: http://{addr}/upload/v1beta/files?upload_id=fixture\r\n"
                        ),
                        String::new(),
                    )
                }
                1 => (
                    String::new(),
                    json!({"file":{"name":name,"mimeType":"video/mp4"}}).to_string(),
                ),
                2 if missing_after_ready => (
                    String::new(),
                    json!({"name":name,"mimeType":"video/mp4","state":"ACTIVE","uri":format!("http://{addr}/media/{name}")}).to_string(),
                ),
                2 | 3 if step != 3 || missing_after_ready => (
                    String::new(),
                    json!({"name":name,"mimeType":"video/mp4","state":"ACTIVE"}).to_string(),
                ),
                _ => (String::new(), String::new()),
            };
                requests.push(request);
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
            (name, requests)
        });
        let config_text = config_doc(&format!("http://{addr}/v1beta"))
            .replace("openai-compatible", "google-genai");
        std::fs::write(&config, format!("{config_text}video_input = true\n")).unwrap();
        let mut video = b"\x00\x00\x00\x10ftypisomtest".to_vec();
        video.resize(llm_router::provider::MAX_INLINE_VIDEO_BYTES + 1, 0);
        std::fs::write(project.join("clip.mp4"), &video).unwrap();
        let result = rapid(&project, &home.0, &config)
            .args(["exec", "Describe @clip.mp4"])
            .output()
            .unwrap();
        assert!(
            !result.status.success(),
            "missing URI unexpectedly succeeded"
        );
        let (name, requests) = server.join().unwrap();
        assert!(
            requests
                .last()
                .unwrap()
                .starts_with(&format!("DELETE /v1beta/{name} HTTP/1.1\r\n"))
        );
        assert!(
            requests
                .iter()
                .all(|request| !request.starts_with("POST /v1beta/models/"))
        );
        let journal = home.0.join(".rapidlm/video-uploads");
        let retained: Vec<_> = std::fs::read_dir(journal)
            .unwrap()
            .filter_map(|entry| {
                let path = entry.unwrap().path();
                (path.extension().is_some_and(|ext| ext == "json"))
                    .then(|| std::fs::read_to_string(path).unwrap())
            })
            .collect();
        assert!(
            retained.is_empty(),
            "retained={retained:?}, stdout={}, stderr={}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

#[test]
fn headless_large_video_recovers_failed_cleanup_on_next_run() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use sha2::Digest as _;
    let home = temp_dir("large-video-recovery");
    let (project, config) = trusted_project(&home.0, |_| answer("unused"));
    let mut video = b"\x00\x00\x00\x10ftypisomtest".to_vec();
    video.resize(llm_router::provider::MAX_INLINE_VIDEO_BYTES + 1, 0);
    let hash = STANDARD.encode(sha2::Sha256::digest(&video));
    let size = video.len();
    std::fs::write(project.join("clip.mp4"), &video).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let mut names = Vec::new();
        let mut current = String::new();
        for index in 0..14 {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(20)))
                .unwrap();
            let request = read_request(&mut stream);
            let (status, extra, body) = match index {
                0 | 8 => {
                    let start: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
                    current = start["file"]["name"].as_str().unwrap().to_owned();
                    names.push(current.clone());
                    (200, format!("X-Goog-Upload-URL: http://{addr}/upload/v1beta/files?upload_id=fixture\r\n"), String::new())
                }
                1 | 9 => (200, String::new(), json!({"file":{"name":current,"mimeType":"video/mp4"}}).to_string()),
                2 | 3 | 10 | 11 => (200, String::new(), json!({"name":current,"mimeType":"video/mp4","state":"ACTIVE","uri":format!("http://{addr}/media/{current}")}).to_string()),
                4 | 12 => (200, String::new(), json!({"candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"received"}]},"finishReason":"STOP"}]}).to_string()),
                5 => { assert!(request.starts_with(&format!("DELETE /v1beta/{current} "))); (500, String::new(), String::new()) },
                6 => { assert!(request.starts_with(&format!("GET /v1beta/{current} "))); (200, String::new(), json!({"name":current,"mimeType":"video/mp4","state":"ACTIVE","sizeBytes":size.to_string(),"sha256Hash":hash}).to_string()) },
                7 => { assert!(request.starts_with(&format!("DELETE /v1beta/{current} "))); (200, String::new(), String::new()) },
                _ => { assert!(request.starts_with(&format!("DELETE /v1beta/{current} "))); (200, String::new(), String::new()) },
            };
            write!(stream, "HTTP/1.1 {status} response\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        }
        names
    });
    let config_text =
        config_doc(&format!("http://{addr}/v1beta")).replace("openai-compatible", "google-genai");
    std::fs::write(&config, format!("{config_text}video_input = true\n")).unwrap();
    let _first = rapid(&project, &home.0, &config)
        .args(["exec", "Describe @clip.mp4"])
        .output()
        .unwrap();
    let journal = home.0.join(".rapidlm/video-uploads");
    assert_eq!(
        std::fs::read_dir(&journal)
            .unwrap()
            .filter(|entry| entry
                .as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|ext| ext == "json"))
            .count(),
        1
    );
    let second = rapid(&project, &home.0, &config)
        .args(["exec", "Describe @clip.mp4"])
        .output()
        .unwrap();
    assert!(
        second.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    let names = server.join().unwrap();
    assert_ne!(names[0], names[1]);
    assert_eq!(
        std::fs::read_dir(journal)
            .unwrap()
            .filter(|entry| entry
                .as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|ext| ext == "json"))
            .count(),
        0
    );
}

#[test]
fn headless_large_video_recovers_when_finalize_response_is_lost() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use sha2::Digest as _;
    let home = temp_dir("large-video-lost-finalize");
    let (project, config) = trusted_project(&home.0, |_| answer("unused"));
    let mut video = b"\x00\x00\x00\x10ftypisomtest".to_vec();
    video.resize(llm_router::provider::MAX_INLINE_VIDEO_BYTES + 1, 0);
    let hash = STANDARD.encode(sha2::Sha256::digest(&video));
    let size = video.len();
    std::fs::write(project.join("clip.mp4"), &video).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let mut name = String::new();
        for index in 0..10 {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(20)))
                .unwrap();
            let request = read_request(&mut stream);
            let (extra, body) = match index {
                0 | 4 => {
                    let start: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
                    name = start["file"]["name"].as_str().unwrap().to_owned();
                    (format!("X-Goog-Upload-URL: http://{addr}/upload/v1beta/files?upload_id=fixture\r\n"), String::new())
                }
                1 => { assert!(request.contains("x-goog-upload-command: upload, finalize\r\n")); continue; },
                2 => { assert!(request.starts_with(&format!("GET /v1beta/{name} "))); (String::new(), json!({"name":name,"mimeType":"video/mp4","state":"ACTIVE","sizeBytes":size.to_string(),"sha256Hash":hash}).to_string()) },
                3 => { assert!(request.starts_with(&format!("DELETE /v1beta/{name} "))); (String::new(), String::new()) },
                5 => (String::new(), json!({"file":{"name":name,"mimeType":"video/mp4"}}).to_string()),
                6 | 7 => (String::new(), json!({"name":name,"mimeType":"video/mp4","state":"ACTIVE","uri":format!("http://{addr}/media/{name}")}).to_string()),
                8 => (String::new(), json!({"candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"received"}]},"finishReason":"STOP"}]}).to_string()),
                _ => { assert!(request.starts_with(&format!("DELETE /v1beta/{name} "))); (String::new(), String::new()) },
            };
            write!(
                stream,
                "HTTP/1.1 200 OK\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        }
    });
    let config_text =
        config_doc(&format!("http://{addr}/v1beta")).replace("openai-compatible", "google-genai");
    std::fs::write(&config, format!("{config_text}video_input = true\n")).unwrap();
    let first = rapid(&project, &home.0, &config)
        .args(["exec", "Describe @clip.mp4"])
        .output()
        .unwrap();
    assert!(!first.status.success());
    let journal = home.0.join(".rapidlm/video-uploads");
    let records: Vec<_> = std::fs::read_dir(&journal)
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            path.extension()
                .is_some_and(|ext| ext == "json")
                .then(|| std::fs::read_to_string(path).unwrap())
        })
        .collect();
    assert_eq!(records.len(), 1);
    let record: Value = serde_json::from_str(&records[0]).unwrap();
    assert_eq!(record["phase"], "finalizing");
    let second = rapid(&project, &home.0, &config)
        .args(["exec", "Describe @clip.mp4"])
        .output()
        .unwrap();
    assert!(
        second.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    server.join().unwrap();
    assert!(std::fs::read_dir(journal).unwrap().all(|entry| {
        !entry
            .unwrap()
            .path()
            .extension()
            .is_some_and(|ext| ext == "json")
    }));
}

#[test]
fn acp_video_attachment_reaches_only_an_enabled_google_model() {
    let home = temp_dir("video-attachment");
    let (project, config) = trusted_project(&home.0, |_| answer("unused"));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        while let Ok((mut stream, _)) = listener.accept() {
            let request = read_request(&mut stream);
            let _ = tx.send(request);
            let body = json!({"candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"video received"}]},"finishReason":"STOP"}]}).to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
        }
    });
    let base_config =
        config_doc(&format!("http://{addr}/v1beta")).replace("openai-compatible", "google-genai");
    std::fs::write(project.join("clip.mp4"), b"\x00\x00\x00\x10ftypisomtest").unwrap();
    for scenario in [
        "enabled",
        "disabled",
        "unsupported",
        "invalid",
        "oversized",
        "missing",
    ] {
        let setting = if scenario == "disabled" {
            "video_input = false\n"
        } else {
            "video_input = true\n"
        };
        let scenario_config = if scenario == "unsupported" {
            base_config.replace("google-genai", "openai-compatible")
        } else {
            base_config.clone()
        };
        std::fs::write(&config, format!("{scenario_config}{setting}")).unwrap();
        let filename = match scenario {
            "missing" => "missing.mp4",
            "invalid" => {
                std::fs::write(project.join("invalid.mp4"), b"not a video").unwrap();
                "invalid.mp4"
            }
            "oversized" => {
                let file = std::fs::File::create(project.join("large.mp4")).unwrap();
                file.set_len(llm_router::providers::google_genai::MAX_UPLOADED_VIDEO_BYTES + 1)
                    .unwrap();
                "large.mp4"
            }
            _ => "clip.mp4",
        };
        let mut editor = Editor::spawn(&project, &home.0, &config);
        let session = editor.open_session(&project);
        let frames = editor.prompt(3, &session, &format!("Describe @{filename}"));
        if scenario == "enabled" {
            assert_eq!(
                frames.last().unwrap()["result"]["stopReason"],
                "end_turn",
                "{frames:?}"
            );
            let request = rx.recv_timeout(Duration::from_secs(3)).unwrap();
            let body: Value =
                serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
            let parts = body["contents"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|item| item["parts"].as_array().unwrap())
                .collect::<Vec<_>>();
            assert!(
                parts
                    .iter()
                    .any(|part| part["inlineData"]["mimeType"] == "video/mp4"
                        && part["inlineData"]["data"] == "AAAAEGZ0eXBpc29tdGVzdA=="),
                "{body}"
            );
        } else {
            assert_eq!(
                frames.last().unwrap()["result"]["stopReason"],
                "refusal",
                "{scenario}: {frames:?}"
            );
            assert!(
                rx.recv_timeout(Duration::from_millis(200)).is_err(),
                "{scenario}: provider received an unsupported video"
            );
        }
        assert_eq!(editor.finish(), Some(0));
    }
    std::fs::write(&config, format!("{base_config}video_input = true\n")).unwrap();
    let run = rapid(&project, &home.0, &config)
        .args(["exec", "Describe @clip.mp4"])
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let request = rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert!(
        body["contents"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|item| item["parts"].as_array().unwrap())
            .any(|part| part["inlineData"]["mimeType"] == "video/mp4"
                && part["inlineData"]["data"] == "AAAAEGZ0eXBpc29tdGVzdA=="),
        "{body}"
    );
    std::fs::write(&config, format!("{base_config}video_input = false\n")).unwrap();
    let refused = rapid(&project, &home.0, &config)
        .args(["exec", "Describe @clip.mp4"])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("video input"));
    assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
}

fn video_then_write(request: &str) -> String {
    if request.contains("functionResponse") {
        if request.contains("AAAAEGZ0eXBpc29tdGVzdA==") {
            if request.matches("functionResponse").count() == 1 {
                return json!({"candidates":[{"index":0,"content":{"role":"model","parts":[{"functionCall":{"id":"video-write-second","name":"workspace_write","args":{"path":"video-second.txt","content":"second"}}}]},"finishReason":"STOP"}]}).to_string();
            }
            return json!({"candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"original video retained"}]},"finishReason":"STOP"}]}).to_string();
        }
        return json!({"candidates":[{"index":0,"content":{"role":"model","parts":[{"functionCall":{"id":"missing-video","name":"workspace_write","args":{"path":"missing-video.txt","content":"missing"}}}]},"finishReason":"STOP"}]}).to_string();
    }
    json!({"candidates":[{"index":0,"content":{"role":"model","parts":[{"functionCall":{"id":"video-write","name":"workspace_write","args":{"path":"video-result.txt","content":"written"}}}]},"finishReason":"STOP"}]}).to_string()
}

#[test]
fn a_video_turn_resumes_with_its_original_attachment_after_the_file_changes() {
    let home = temp_dir("video-approval");
    let (project, config) = trusted_project(&home.0, video_then_write);
    let text = std::fs::read_to_string(&config)
        .unwrap()
        .replace("openai-compatible", "google-genai");
    std::fs::write(&config, format!("{text}video_input = true\n")).unwrap();
    std::fs::write(project.join("clip.mp4"), b"\x00\x00\x00\x10ftypisomtest").unwrap();
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);
    editor.start_prompt(3, &session, "Inspect @clip.mp4 and write a result");
    let permission = editor.until_permission();
    assert_eq!(
        permission["params"]["toolCall"]["toolCallId"],
        "video-write"
    );
    std::fs::write(project.join("clip.mp4"), b"\x00\x00\x00\x10ftypisomchanged").unwrap();
    editor.answer_permission(
        &permission,
        json!({"outcome":{"outcome":"selected","optionId":"allow-once"}}),
    );
    let frames = editor.play(3);
    assert_eq!(
        frames.last().unwrap()["result"]["stopReason"],
        "end_turn",
        "{frames:?}"
    );
    assert_eq!(
        std::fs::read_to_string(project.join("video-result.txt")).unwrap(),
        "written"
    );
    assert_eq!(
        std::fs::read_to_string(project.join("video-second.txt")).unwrap(),
        "second"
    );
    assert!(!project.join("missing-video.txt").exists());
    assert_eq!(editor.finish(), Some(0));
}

fn video_then_question(request: &str) -> String {
    if request.contains("functionResponse") {
        let text = if request.contains("AAAAEGZ0eXBpc29tdGVzdA==") {
            "ORIGINAL_VIDEO_RESTORED"
        } else {
            "VIDEO_MISSING"
        };
        return json!({"candidates":[{"index":0,"content":{"role":"model","parts":[{"text":text}]},"finishReason":"STOP"}]}).to_string();
    }
    json!({"candidates":[{"index":0,"content":{"role":"model","parts":[{"functionCall":{"id":"video-question","name":"ask_user","args":{"version":1,"questions":[{"id":"q","question":"Proceed?","options":[{"id":"yes","label":"Yes"},{"id":"no","label":"No"}]}]}}}]},"finishReason":"STOP"}]}).to_string()
}

#[test]
fn video_question_after_host_restart_restores_only_matching_bytes() {
    for scenario in ["unchanged", "identical", "changed", "missing", "outside"] {
        if scenario == "outside" && !cfg!(unix) {
            continue;
        }
        let home = temp_dir("video-restart");
        let (project, config) = trusted_project(&home.0, video_then_question);
        let text = std::fs::read_to_string(&config)
            .unwrap()
            .replace("openai-compatible", "google-genai");
        std::fs::write(&config, format!("{text}video_input = true\n")).unwrap();
        std::fs::write(project.join("clip.mp4"), b"\x00\x00\x00\x10ftypisomtest").unwrap();
        let mut editor = Editor::spawn(&project, &home.0, &config);
        editor.request(1, "initialize", json!({"protocolVersion":2,"info":{"name":"test-editor","version":"1"},"capabilities":{"elicitation":{"form":{}}}}));
        assert!(editor.until_result(1).get("result").is_some());
        editor.request(
            2,
            "session/new",
            json!({"cwd":project.display().to_string(),"mcpServers":[]}),
        );
        let session = editor.until_result(2)["result"]["sessionId"]
            .as_str()
            .unwrap()
            .to_owned();
        editor.start_prompt(3, &session, "Inspect @clip.mp4 and ask");
        let deadline = Instant::now() + DEADLINE;
        let mut found = false;
        while let Some(frame) = editor.next(deadline) {
            if frame["method"] == "elicitation/create" {
                found = true;
                break;
            }
        }
        assert!(found);
        assert_eq!(editor.finish(), Some(0));
        match scenario {
            "identical" => {
                std::fs::write(project.join("new.mp4"), b"\x00\x00\x00\x10ftypisomtest").unwrap();
                std::fs::rename(project.join("new.mp4"), project.join("clip.mp4")).unwrap();
            }
            "changed" => {
                std::fs::write(project.join("clip.mp4"), b"\x00\x00\x00\x10ftypisomchanged")
                    .unwrap()
            }
            "missing" => std::fs::remove_file(project.join("clip.mp4")).unwrap(),
            #[cfg(unix)]
            "outside" => {
                std::fs::write(home.0.join("outside.mp4"), b"\x00\x00\x00\x10ftypisomtest")
                    .unwrap();
                std::fs::remove_file(project.join("clip.mp4")).unwrap();
                std::os::unix::fs::symlink(home.0.join("outside.mp4"), project.join("clip.mp4"))
                    .unwrap();
            }
            _ => {}
        }
        let listed = rapid(&project, &home.0, &config)
            .args(["questions", "list", &session])
            .output()
            .unwrap();
        assert!(listed.status.success());
        let listed: Value = serde_json::from_slice(&listed.stdout).unwrap();
        let token = listed["items"][0]["token"].as_str().unwrap();
        let seq = listed["expected_seq"].as_u64().unwrap().to_string();
        let mut process = rapid(&project, &home.0, &config)
            .args(["questions", "answer", &session, token, &seq])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        process.stdin.take().unwrap().write_all(br#"{"version":1,"status":"complete","answers":[{"question_id":"q","selected":["yes"]}]}"#).unwrap();
        let result = process.wait_with_output().unwrap();
        let outcome: Value = serde_json::from_slice(&result.stdout).unwrap();
        if matches!(scenario, "changed" | "missing" | "outside") {
            assert!(!result.status.success());
            assert_eq!(outcome["status"], "failed", "{outcome}");
            assert!(
                outcome
                    .to_string()
                    .contains("original video attachment is unavailable or changed"),
                "{outcome}"
            );
        } else {
            assert!(result.status.success(), "{outcome}");
            assert_eq!(outcome["text"], "ORIGINAL_VIDEO_RESTORED", "{outcome}");
        }
    }
}

#[test]
fn headless_video_question_is_answerable_after_the_exec_process_exits() {
    let home = temp_dir("headless-video-question");
    let (project, config) = trusted_project(&home.0, video_then_question);
    let text = std::fs::read_to_string(&config)
        .unwrap()
        .replace("openai-compatible", "google-genai");
    std::fs::write(&config, format!("{text}video_input = true\n")).unwrap();
    std::fs::write(project.join("clip.mp4"), b"\x00\x00\x00\x10ftypisomtest").unwrap();
    let run = rapid(&project, &home.0, &config)
        .args(["exec", "Inspect @clip.mp4 and ask"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&run.stderr);
    let session = stderr
        .lines()
        .find_map(|line| {
            line.strip_prefix("session ")
                .and_then(|rest| rest.split_whitespace().next())
        })
        .unwrap_or_else(|| panic!("missing session: {stderr}"));
    let listing = rapid(&project, &home.0, &config)
        .args(["questions", "list", session])
        .output()
        .unwrap();
    assert!(
        listing.status.success(),
        "{}",
        String::from_utf8_lossy(&listing.stderr)
    );
    let listing: Value = serde_json::from_slice(&listing.stdout).unwrap();
    assert_eq!(
        listing["items"].as_array().unwrap().len(),
        1,
        "{listing}; {stderr}"
    );
    let token = listing["items"][0]["token"].as_str().unwrap();
    let seq = listing["expected_seq"].as_u64().unwrap().to_string();
    let mut child = rapid(&project, &home.0, &config)
        .args(["questions", "answer", session, token, &seq])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(br#"{"version":1,"status":"complete","answers":[{"question_id":"q","selected":["yes"]}]}"#).unwrap();
    let result = child.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "{} {}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    let outcome: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(outcome["text"], "ORIGINAL_VIDEO_RESTORED", "{outcome}");
}

fn structured_question_model(request: &str) -> String {
    let direct_history_check = request
        .split_once("\r\n\r\n")
        .and_then(|(_, body)| serde_json::from_str::<Value>(body).ok())
        .and_then(|body| body["messages"].as_array().cloned())
        .is_some_and(|messages| {
            messages
                .iter()
                .any(|message| message["role"] == "user" && message["content"] == "HISTORY_CHECK")
        });
    if direct_history_check {
        let has_question = request
            .split_once("\r\n\r\n")
            .and_then(|(_, body)| serde_json::from_str::<Value>(body).ok())
            .and_then(|body| body["messages"].as_array().cloned())
            .is_some_and(|messages| {
                messages.iter().any(|message| {
                    message["content"].as_str().is_some_and(|content| {
                        content.contains("[assistant]")
                            && content.contains("Which?")
                            && content.contains("Where?")
                    })
                })
            });
        return if tool_results(request) == 0 && has_question {
            tool_call(
                "history-proof",
                "workspace_write",
                json!({"path":"history-proof.txt","content":"question visible"}),
            )
        } else {
            answer("history checked")
        };
    }
    let answer_contains_choices = request
        .split_once("\r\n\r\n")
        .and_then(|(_, body)| serde_json::from_str::<Value>(body).ok())
        .and_then(|body| body["messages"].as_array().cloned())
        .is_some_and(|messages| {
            messages.iter().any(|message| {
                message["tool_call_id"] == "form-1"
                    && message["content"].as_str().is_some_and(|content| {
                        content.contains("custom destination")
                            && content.contains("\"selected\":[\"a\",\"b\"]")
                    })
            })
        });
    if tool_results(request) == 0 {
        tool_call(
            "form-1",
            "ask_user",
            json!({"version":1,"questions":[
                {"id":"one","question":"Which?","multi_select":true,"options":[{"id":"a","label":"A"},{"id":"b","label":"B"}]},
                {"id":"two","question":"Where?","options":[{"id":"x","label":"X"},{"id":"y","label":"Y"}]}
            ]}),
        )
    } else if tool_results(request) == 1 && answer_contains_choices {
        tool_call(
            "form-proof",
            "workspace_write",
            json!({"path":"form-proof.txt","content":"FORM_ANSWER_RECEIVED"}),
        )
    } else if tool_results(request) > 1 {
        answer("done")
    } else {
        answer("FORM_ANSWER_MISSING")
    }
}

fn question_after_read_model(request: &str) -> String {
    match tool_results(request) {
        0 => tool_call("question-read", "repo_read", json!({"path":"notes.txt"})),
        1 => tool_call(
            "question-after-read",
            "ask_user",
            json!({"version":1,"questions":[{"id":"q","question":"Proceed?","options":[{"id":"yes","label":"Yes"},{"id":"no","label":"No"}]}]}),
        ),
        2 => tool_call(
            "question-write",
            "workspace_write",
            json!({"path":"notes.txt","content":"after answer"}),
        ),
        _ => answer("done"),
    }
}

#[test]
fn acp_question_continuation_reuses_only_fresh_read_authority() {
    for scenario in ["fresh", "changed", "identical-replacement", "restart"] {
        let home = temp_dir("question-read-authority");
        let (project, config) = trusted_project(&home.0, question_after_read_model);
        std::fs::write(project.join("notes.txt"), "before").unwrap();
        let mut editor = Editor::spawn(&project, &home.0, &config);
        editor.request(1, "initialize", json!({"protocolVersion":2,"info":{"name":"test-editor","version":"1"},"capabilities":{"elicitation":{"form":{}}}}));
        assert!(editor.until_result(1).get("result").is_some());
        editor.request(
            2,
            "session/new",
            json!({"cwd":project.display().to_string(),"mcpServers":[]}),
        );
        let session = editor.until_result(2)["result"]["sessionId"]
            .as_str()
            .unwrap()
            .to_owned();
        editor.start_prompt(3, &session, "Read, ask, and update");
        let deadline = Instant::now() + DEADLINE;
        let mut saw_form = false;
        let mut frames = Vec::new();
        while let Some(frame) = editor.next(deadline) {
            if frame["method"] == "elicitation/create" {
                saw_form = true;
                if scenario == "changed" {
                    std::fs::write(project.join("notes.txt"), "external").unwrap();
                } else if scenario == "identical-replacement" {
                    std::fs::write(project.join("replacement.txt"), "before").unwrap();
                    std::fs::rename(project.join("replacement.txt"), project.join("notes.txt"))
                        .unwrap();
                }
                if scenario == "restart" {
                    frames.push(frame);
                    break;
                }
                editor.send(json!({"jsonrpc":"2.0","id":frame["id"],"result":{"action":"accept","content":{"q0_choice":"yes"}}}));
            } else if frame["method"] == "session/request_permission" {
                editor.answer_permission(
                    &frame,
                    json!({"outcome":{"outcome":"selected","optionId":"allow-once"}}),
                );
            }
            let done = frame["id"] == 3 && frame.get("method").is_none();
            frames.push(frame);
            if done {
                break;
            }
        }
        assert!(saw_form, "{frames:?}");
        if scenario == "restart" {
            assert_eq!(editor.finish(), Some(0));
            let list = rapid(&project, &home.0, &config)
                .args(["questions", "list", &session])
                .output()
                .unwrap();
            assert!(
                list.status.success(),
                "{}",
                String::from_utf8_lossy(&list.stderr)
            );
            let listed: Value = serde_json::from_slice(&list.stdout).unwrap();
            let token = listed["items"][0]["token"].as_str().unwrap();
            let seq = listed["expected_seq"].as_u64().unwrap().to_string();
            let mut process = rapid(&project, &home.0, &config)
                .env("RAPIDLM_PERMISSION_MODE", "acceptEdits")
                .args(["questions", "answer", &session, token, &seq])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            process.stdin.take().unwrap().write_all(br#"{"version":1,"status":"complete","answers":[{"question_id":"q","selected":["yes"]}]}"#).unwrap();
            let result = process.wait_with_output().unwrap();
            assert!(
                result.status.success(),
                "{} {}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            let outcome: Value = serde_json::from_slice(&result.stdout).unwrap();
            assert_eq!(outcome["text"], "done", "{outcome}");
            assert_eq!(
                std::fs::read_to_string(project.join("notes.txt")).unwrap(),
                "before"
            );
            continue;
        }
        assert_eq!(
            frames.last().unwrap()["result"]["stopReason"],
            "end_turn",
            "{frames:?}"
        );
        assert!(
            format!("{frames:?}").contains("question-write"),
            "{scenario}: {frames:?}"
        );
        assert_eq!(
            std::fs::read_to_string(project.join("notes.txt")).unwrap(),
            match scenario {
                "changed" => "external",
                "identical-replacement" => "before",
                _ => "after answer",
            },
            "{frames:?}"
        );
        assert_eq!(editor.finish(), Some(0));
    }
}

#[test]
fn acp_real_model_round_trip_accepts_a_multi_question_form() {
    let home = temp_dir("structured-form");
    let (project, config) = trusted_project(&home.0, structured_question_model);
    let mut editor = Editor::spawn(&project, &home.0, &config);
    editor.request(
        1,
        "initialize",
        json!({
            "protocolVersion":2,
            "info":{"name":"test-editor","version":"1"},
            "capabilities":{"elicitation":{"form":{}}}
        }),
    );
    let initialized = editor.until_result(1);
    assert!(initialized.get("result").is_some(), "{initialized}");
    editor.request(
        2,
        "session/new",
        json!({"cwd":project.display().to_string(),"mcpServers":[]}),
    );
    let created = editor.until_result(2);
    let session = created["result"]["sessionId"].as_str().unwrap().to_owned();
    editor.start_prompt(3, &session, "Ask me for choices");
    let deadline = Instant::now() + DEADLINE;
    let mut saw_form = false;
    let mut frames = Vec::new();
    while let Some(frame) = editor.next(deadline) {
        if frame["method"] == "elicitation/create" {
            assert!(!saw_form, "unexpected repeated form: {frame}");
            saw_form = true;
            let schema = &frame["params"]["requestedSchema"];
            assert!(schema["properties"].get("q0_choice").is_some(), "{schema}");
            assert!(schema["properties"].get("q1_other").is_some(), "{schema}");
            editor.send(json!({"jsonrpc":"2.0","id":frame["id"],"result":{
                "action":"accept","content":{"q0_choice":["a","b"],"q1_other":"custom destination"}
            }}));
        } else if frame["method"] == "session/request_permission" {
            editor.answer_permission(
                &frame,
                json!({"outcome":{"outcome":"selected","optionId":"allow-once"}}),
            );
        }
        let done = frame["id"] == 3 && frame.get("method").is_none();
        frames.push(frame);
        if done {
            break;
        }
    }
    assert!(
        saw_form,
        "no elicitation: {frames:?}; {}",
        editor.stderr_text()
    );
    assert_eq!(
        frames.last().unwrap()["result"]["stopReason"],
        "end_turn",
        "{frames:?}"
    );
    assert_eq!(
        std::fs::read_to_string(project.join("form-proof.txt")).unwrap(),
        "FORM_ANSWER_RECEIVED",
        "{frames:?}"
    );
    assert_eq!(editor.finish(), Some(0));
}

#[test]
fn acp_unanswered_forms_remain_available_to_the_headless_cli() {
    for scenario in ["cancel", "invalid", "unsupported"] {
        let home = temp_dir("form-handoff");
        let (project, config) = trusted_project(&home.0, structured_question_model);
        let mut editor = Editor::spawn(&project, &home.0, &config);
        let session = if scenario == "unsupported" {
            editor.open_session(&project)
        } else {
            editor.request(1, "initialize", json!({"protocolVersion":2,"info":{"name":"test-editor","version":"1"},"capabilities":{"elicitation":{"form":{}}}}));
            assert!(editor.until_result(1).get("result").is_some());
            editor.request(
                2,
                "session/new",
                json!({"cwd":project.display().to_string(),"mcpServers":[]}),
            );
            editor.until_result(2)["result"]["sessionId"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        editor.start_prompt(3, &session, "Ask me for choices");
        let deadline = Instant::now() + DEADLINE;
        let mut saw_form = false;
        let mut frames = Vec::new();
        while let Some(frame) = editor.next(deadline) {
            if frame["method"] == "elicitation/create" {
                saw_form = true;
                let result = if scenario == "cancel" {
                    json!({"action":"cancel"})
                } else {
                    json!({"action":"accept","content":{"q0_choice":["bogus"],"q1_other":"custom destination"}})
                };
                editor.send(json!({"jsonrpc":"2.0","id":frame["id"],"result":result}));
            }
            let done = frame["id"] == 3 && frame.get("method").is_none();
            frames.push(frame);
            if done {
                break;
            }
        }
        assert_eq!(
            saw_form,
            scenario != "unsupported",
            "{scenario}: {frames:?}"
        );
        assert_eq!(
            frames.last().unwrap()["result"]["stopReason"],
            if scenario == "cancel" {
                "cancelled"
            } else {
                "refusal"
            },
            "{scenario}: {frames:?}"
        );
        if scenario == "cancel" {
            let history_frames = editor.prompt(4, &session, "HISTORY_CHECK");
            assert_eq!(
                history_frames.last().unwrap()["result"]["stopReason"],
                "end_turn",
                "{history_frames:?}"
            );
            assert_eq!(
                std::fs::read_to_string(project.join("history-proof.txt")).unwrap(),
                "question visible",
                "{history_frames:?}"
            );
        }
        assert_eq!(editor.finish(), Some(0));
        assert!(!project.join("form-proof.txt").exists());

        let list = rapid(&project, &home.0, &config)
            .args(["questions", "list", &session])
            .output()
            .unwrap();
        assert!(
            list.status.success(),
            "{scenario}: {}",
            String::from_utf8_lossy(&list.stderr)
        );
        let listed: Value = serde_json::from_slice(&list.stdout).unwrap();
        assert_eq!(listed["items"].as_array().unwrap().len(), 1, "{listed}");
        let token = listed["items"][0]["token"].as_str().unwrap();
        let seq = listed["expected_seq"].as_u64().unwrap().to_string();
        let submit = |sequence: &str, body: &[u8]| {
            let mut process = rapid(&project, &home.0, &config)
                .env("RAPIDLM_PERMISSION_MODE", "acceptEdits")
                .args(["questions", "answer", &session, token, sequence])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            process.stdin.take().unwrap().write_all(body).unwrap();
            process.wait_with_output().unwrap()
        };
        let partial = br#"{"version":1,"status":"partial","answers":[{"question_id":"one","selected":["a","b"]}]}"#;
        let complete = br#"{"version":1,"status":"complete","answers":[{"question_id":"one","selected":["a","b"]},{"question_id":"two","selected":[],"other":"custom destination"}]}"#;
        let saved = submit(&seq, partial);
        assert!(
            saved.status.success(),
            "{scenario}: {}",
            String::from_utf8_lossy(&saved.stderr)
        );
        assert!(
            !submit(&seq, complete).status.success(),
            "{scenario}: stale answer accepted"
        );
        assert!(!project.join("form-proof.txt").exists());
        let refreshed = rapid(&project, &home.0, &config)
            .args(["questions", "list", &session])
            .output()
            .unwrap();
        assert!(refreshed.status.success());
        let refreshed: Value = serde_json::from_slice(&refreshed.stdout).unwrap();
        assert_eq!(
            refreshed["items"][0]["progress"]["answers"][0]["selected"],
            json!(["a", "b"])
        );
        let fresh_seq = refreshed["expected_seq"].as_u64().unwrap().to_string();
        let result = submit(&fresh_seq, complete);
        assert!(
            result.status.success(),
            "{scenario}: {} {}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(project.join("form-proof.txt")).unwrap(),
            "FORM_ANSWER_RECEIVED"
        );
        let final_list = rapid(&project, &home.0, &config)
            .args(["questions", "list", &session])
            .output()
            .unwrap();
        assert!(final_list.status.success());
        assert!(
            serde_json::from_slice::<Value>(&final_list.stdout).unwrap()["items"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
}

/// How many tool results the model has been shown so far.
fn tool_results(request: &str) -> usize {
    request.matches("\"tool_call_id\"").count()
}

fn read_then_approved_edit(request: &str) -> String {
    match tool_results(request) {
        0 => tool_call("read-1", "repo_read", json!({"path":"notes.txt"})),
        1 => tool_call(
            "write-1",
            "workspace_write",
            json!({"path":"notes.txt","content":"after"}),
        ),
        _ => answer("done"),
    }
}

#[cfg(unix)]
fn recovered_read_then_edit(request: &str) -> String {
    match tool_results(request) {
        0 => tool_call("read-recovered", "repo_read", json!({"path":"notes.txt"})),
        1 if request.contains("alpha\\n") => tool_call(
            "write-recovered",
            "workspace_write",
            json!({"path":"notes.txt","content":"after"}),
        ),
        1 => answer("RECOVERY_NOT_VISIBLE"),
        _ => answer("done"),
    }
}

#[cfg(unix)]
#[test]
fn acp_preflights_recovery_before_model_and_stops_on_conflict() {
    use std::os::unix::fs::PermissionsExt;
    use workspace::multi_edit_journal::JournalStore;
    use workspace::multi_edit_recovery::{FileImage, RecoveryEntry, RecoveryRecord, RecoveryState};
    for scenario in ["recover", "conflict"] {
        let home = temp_dir("preflight-recovery");
        let (project, config) = trusted_project(&home.0, recovered_read_then_edit);
        let original = if scenario == "recover" {
            "beta\n"
        } else {
            "external\n"
        };
        std::fs::write(project.join("notes.txt"), original).unwrap();
        std::fs::set_permissions(
            project.join("notes.txt"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let dir = project.join(".rapidlm/multi-edit-journal");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let journal = JournalStore::lock(&dir).unwrap();
        let image = |text: &str| {
            Some(FileImage {
                bytes: text.as_bytes().to_vec(),
                mode: Some(0o600),
            })
        };
        journal
            .write_intent(
                &RecoveryRecord::new(
                    RecoveryState::Intent,
                    vec![RecoveryEntry {
                        path: "notes.txt".into(),
                        before: image("alpha\n"),
                        after: image("beta\n"),
                    }],
                )
                .unwrap(),
            )
            .unwrap();
        drop(journal);
        let mut editor = Editor::spawn(&project, &home.0, &config);
        let session = editor.open_session(&project);
        let frames = editor.prompt(3, &session, "read and update notes");
        if scenario == "recover" {
            assert_eq!(
                frames.last().unwrap()["result"]["stopReason"],
                "end_turn",
                "{frames:?}"
            );
            assert!(!format!("{frames:?}").contains("RECOVERY_NOT_VISIBLE"));
            assert_eq!(
                std::fs::read_to_string(project.join("notes.txt")).unwrap(),
                "after"
            );
            assert_eq!(JournalStore::lock(&dir).unwrap().load().unwrap(), None);
        } else {
            assert_eq!(
                frames.last().unwrap()["result"]["stopReason"],
                "refusal",
                "{frames:?}"
            );
            assert_eq!(
                std::fs::read_to_string(project.join("notes.txt")).unwrap(),
                original
            );
            assert!(JournalStore::lock(&dir).unwrap().load().unwrap().is_some());
        }
        assert_eq!(editor.finish(), Some(0));
    }
}

fn read_then_multi_patch(request: &str) -> String {
    match tool_results(request) {
        0 => tool_call("read-multi", "repo_read", json!({"path":"notes.txt"})),
        1 => tool_call(
            "patch-multi",
            "workspace_patch",
            json!({"path":"notes.txt","edits":[{"old":"before","new":"middle"},{"old":"middle","new":"after"}]}),
        ),
        _ => answer("done"),
    }
}

#[cfg(unix)]
fn read_two_then_multi_file_patch(request: &str) -> String {
    match tool_results(request) {
        0 => tool_call("read-a", "repo_read", json!({"path":"notes.txt"})),
        1 => tool_call("read-b", "repo_read", json!({"path":"other.txt"})),
        2 => tool_call(
            "patch-files",
            "workspace_multi_patch",
            json!({"files":[
                {"path":"notes.txt","edits":[{"old":"before","new":"after"}]},
                {"path":"other.txt","edits":[{"old":"second","new":"changed"}]}
            ]}),
        ),
        _ => answer("done"),
    }
}

#[cfg(unix)]
fn read_then_create_and_delete(request: &str) -> String {
    match tool_results(request) {
        0 => tool_call("read-delete", "repo_read", json!({"path":"notes.txt"})),
        1 => tool_call(
            "create-delete",
            "workspace_multi_patch",
            json!({"files":[
                {"path":"new.txt","create":"new content"}, {"path":"notes.txt","delete":true}
            ]}),
        ),
        _ => answer("done"),
    }
}

#[cfg(unix)]
#[test]
fn acp_create_and_delete_have_separate_action_previews() {
    use std::os::unix::fs::PermissionsExt;
    for scenario in ["approved", "denied"] {
        let home = temp_dir(&format!("multi-create-acp-{scenario}"));
        let (project, config) = trusted_project(&home.0, read_then_create_and_delete);
        std::fs::write(project.join("notes.txt"), "before").unwrap();
        let mut editor = Editor::spawn(&project, &home.0, &config);
        let session = editor.open_session(&project);
        editor.start_prompt(3, &session, "create and delete");
        let first = editor.until_permission();
        let preview = first["params"]["toolCall"]["content"][0]["content"]["text"]
            .as_str()
            .unwrap();
        assert!(
            preview.contains("create")
                && preview.contains("new.txt")
                && preview.contains("0600")
                && preview.contains("new content"),
            "{preview}"
        );
        editor.answer_permission(
            &first,
            json!({"outcome":{"outcome":"selected","optionId":"allow-once"}}),
        );
        let second = editor.until_permission();
        let preview = second["params"]["toolCall"]["content"][0]["content"]["text"]
            .as_str()
            .unwrap();
        assert!(
            preview.contains("delete")
                && preview.contains("notes.txt")
                && preview.contains("before"),
            "{preview}"
        );
        assert!(!project.join("new.txt").exists());
        editor.answer_permission(&second, json!({"outcome":{"outcome":"selected","optionId":if scenario == "approved" { "allow-once" } else { "reject-once" }}}));
        let frames = editor.play(3);
        assert_eq!(
            frames.last().unwrap()["result"]["stopReason"],
            "end_turn",
            "{scenario}: {frames:?}"
        );
        if scenario == "approved" {
            assert!(!project.join("notes.txt").exists());
            assert_eq!(
                std::fs::read_to_string(project.join("new.txt")).unwrap(),
                "new content"
            );
            assert_eq!(
                std::fs::metadata(project.join("new.txt"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        } else {
            assert_eq!(
                std::fs::read_to_string(project.join("notes.txt")).unwrap(),
                "before"
            );
            assert!(!project.join("new.txt").exists());
        }
        assert_eq!(editor.finish(), Some(0));
    }
}

#[cfg(unix)]
#[test]
fn acp_multi_file_patch_asks_per_path_and_rechecks_before_commit() {
    for scenario in ["fresh", "denied", "changed"] {
        let home = temp_dir(&format!("multi-file-acp-{scenario}"));
        let (project, config) = trusted_project(&home.0, read_two_then_multi_file_patch);
        std::fs::write(project.join("notes.txt"), "before").unwrap();
        std::fs::write(project.join("other.txt"), "second").unwrap();
        let mut editor = Editor::spawn(&project, &home.0, &config);
        let session = editor.open_session(&project);
        editor.start_prompt(3, &session, "patch both files");
        let first = editor.until_permission();
        let first_preview = first["params"]["toolCall"]["content"][0]["content"]["text"]
            .as_str()
            .unwrap();
        assert!(
            first_preview.contains("notes.txt")
                && first_preview.contains("before")
                && first_preview.contains("after"),
            "{first_preview}"
        );
        editor.answer_permission(
            &first,
            json!({"outcome":{"outcome":"selected","optionId":"allow-once"}}),
        );
        let second = editor.until_permission();
        let second_preview = second["params"]["toolCall"]["content"][0]["content"]["text"]
            .as_str()
            .unwrap();
        assert!(
            second_preview.contains("other.txt")
                && second_preview.contains("second")
                && second_preview.contains("changed"),
            "{second_preview}"
        );
        assert_eq!(
            std::fs::read_to_string(project.join("notes.txt")).unwrap(),
            "before"
        );
        if scenario == "changed" {
            std::fs::write(project.join("other.txt"), "external").unwrap();
        }
        let decision = if scenario == "denied" {
            "reject-once"
        } else {
            "allow-once"
        };
        editor.answer_permission(
            &second,
            json!({"outcome":{"outcome":"selected","optionId":decision}}),
        );
        let frames = editor.play(3);
        assert_eq!(
            frames.last().unwrap()["result"]["stopReason"],
            "end_turn",
            "{scenario}: {frames:?}"
        );
        assert_eq!(
            std::fs::read_to_string(project.join("notes.txt")).unwrap(),
            if scenario == "fresh" {
                "after"
            } else {
                "before"
            }
        );
        assert_eq!(
            std::fs::read_to_string(project.join("other.txt")).unwrap(),
            match scenario {
                "fresh" => "changed",
                "changed" => "external",
                _ => "second",
            }
        );
        assert_eq!(editor.finish(), Some(0));
    }
}

#[cfg(unix)]
#[test]
fn acp_multi_file_hook_rewrite_survives_approvals_and_denial_recovery() {
    use std::os::unix::fs::PermissionsExt;
    for scenario in ["approved", "denied"] {
        let home = temp_dir(&format!("multi-hook-acp-{scenario}"));
        let (project, config) = trusted_project(&home.0, read_two_then_multi_file_patch);
        std::fs::write(project.join("notes.txt"), "before").unwrap();
        std::fs::write(project.join("other.txt"), "second").unwrap();
        std::fs::write(project.join("pre-hook.sh"), "printf x >> hook-count\nprintf '%s\\n' '{\"decision\":\"ask\",\"reason\":\"review rewrite\",\"additional_context\":\"hook context\",\"updated_input\":{\"files\":[{\"path\":\"other.txt\",\"edits\":[{\"old\":\"second\",\"new\":\"changed\"}]}]}}'\n").unwrap();
        std::fs::write(project.join(".rapidlm/settings.json"), json!({
            "formatters":[{"command":["sh","-c","printf x >> format-count; tr a-z A-Z"],"globs":["*.txt"]}],
            "hooks":{"pre_tool_use":[{"command":"sh pre-hook.sh","matcher":"workspace_multi_patch"}]}
        }).to_string()).unwrap();
        let mut editor = Editor::spawn(&project, &home.0, &config);
        let session = editor.open_session(&project);
        editor.start_prompt(3, &session, "patch both files");
        for (index, expected) in [
            "notes.txt",
            "other.txt",
            "hook question",
            "pre-use hook",
            "formatter may run",
            "formatted content",
        ]
        .into_iter()
        .enumerate()
        {
            let request = editor.until_permission();
            let preview = request["params"]["toolCall"]["content"][0]["content"]["text"]
                .as_str()
                .unwrap();
            assert!(
                preview.contains(expected),
                "{scenario} stage {index}: {preview}"
            );
            if index < 2 {
                assert!(!project.join("hook-count").exists());
            }
            if index < 5 {
                assert!(!project.join("format-count").exists());
            }
            if index == 5 && scenario == "denied" {
                let journal = project.join(".rapidlm/multi-edit-journal");
                let entries: Vec<_> = std::fs::read_dir(&journal)
                    .unwrap()
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .collect();
                let hook = entries
                    .iter()
                    .find(|path| {
                        path.file_name()
                            .unwrap()
                            .to_string_lossy()
                            .starts_with("hook-")
                            && path.extension().is_some_and(|ext| ext == "json")
                    })
                    .unwrap()
                    .clone();
                let pending = entries
                    .iter()
                    .find(|path| {
                        path.file_name()
                            .unwrap()
                            .to_string_lossy()
                            .ends_with(".pending")
                    })
                    .unwrap()
                    .clone();
                let saved_hook = std::fs::read(&hook).unwrap();
                let saved_pending = std::fs::read(&pending).unwrap();
                editor.answer_permission(
                    &request,
                    json!({"outcome":{"outcome":"selected","optionId":"reject-once"}}),
                );
                let frames = editor.play(3);
                assert_eq!(
                    frames.last().unwrap()["result"]["stopReason"],
                    "end_turn",
                    "{frames:?}"
                );
                assert_eq!(
                    std::fs::read(&hook).unwrap(),
                    b"rapidlm.multi-hook.retired.v1\n"
                );
                assert!(!pending.exists());
                assert_eq!(editor.finish(), Some(0));
                std::fs::write(&hook, saved_hook).unwrap();
                std::fs::write(&pending, saved_pending).unwrap();
                std::fs::set_permissions(&pending, std::fs::Permissions::from_mode(0o600)).unwrap();
                let mut reopened = Editor::spawn(&project, &home.0, &config);
                reopened.request(1, "initialize", json!({"protocolVersion":1}));
                assert!(reopened.until_result(1).get("result").is_some());
                reopened.request(2, "session/load", json!({"sessionId":session,"cwd":project.display().to_string(),"mcpServers":[]}));
                let loaded = reopened.until_result(2);
                assert!(loaded.get("result").is_some(), "{loaded}");
                assert_eq!(
                    std::fs::read(&hook).unwrap(),
                    b"rapidlm.multi-hook.retired.v1\n"
                );
                assert!(!pending.exists());
                assert_eq!(reopened.finish(), Some(0));
                break;
            }
            editor.answer_permission(
                &request,
                json!({"outcome":{"outcome":"selected","optionId":"allow-once"}}),
            );
            if index == 5 {
                let frames = editor.play(3);
                assert_eq!(
                    frames.last().unwrap()["result"]["stopReason"],
                    "end_turn",
                    "{frames:?}"
                );
                assert_eq!(editor.finish(), Some(0));
                break;
            }
        }
        assert_eq!(std::fs::read(project.join("hook-count")).unwrap(), b"x");
        assert_eq!(std::fs::read(project.join("format-count")).unwrap(), b"x");
        assert_eq!(
            std::fs::read_to_string(project.join("notes.txt")).unwrap(),
            "before"
        );
        assert_eq!(
            std::fs::read_to_string(project.join("other.txt")).unwrap(),
            if scenario == "approved" {
                "CHANGED"
            } else {
                "second"
            }
        );
    }
}

#[cfg(unix)]
#[test]
fn acp_multi_file_formatter_requires_approval_of_the_final_bytes() {
    for scenario in ["approved", "denied", "denied-changed-config"] {
        let home = temp_dir(&format!("multi-format-acp-{scenario}"));
        let (project, config) = trusted_project(&home.0, read_two_then_multi_file_patch);
        std::fs::write(project.join("notes.txt"), "before").unwrap();
        std::fs::write(project.join("other.txt"), "second").unwrap();
        std::fs::write(
            project.join("post-hook.sh"),
            "cat > post-hook.json; echo post-hook-output\n",
        )
        .unwrap();
        std::fs::write(project.join(".rapidlm/settings.json"), json!({
            "formatters": [{"command":["sh","-c","printf x >> format-count; tr a-z A-Z"],"globs":["*.txt"]}],
            "hooks": {"post_tool_use":[{"command":"sh post-hook.sh", "matcher":"workspace_multi_patch"}]}
        }).to_string()).unwrap();
        let mut editor = Editor::spawn(&project, &home.0, &config);
        let session = editor.open_session(&project);
        editor.start_prompt(3, &session, "patch both files");
        for _ in 0..2 {
            let request = editor.until_permission();
            let preview = request["params"]["toolCall"]["content"][0]["content"]["text"]
                .as_str()
                .unwrap();
            assert!(preview.contains("formatter may run"), "{preview}");
            assert!(!project.join("format-count").exists());
            editor.answer_permission(
                &request,
                json!({"outcome":{"outcome":"selected","optionId":"allow-once"}}),
            );
        }
        let final_first = editor.until_permission();
        let preview = final_first["params"]["toolCall"]["content"][0]["content"]["text"]
            .as_str()
            .unwrap();
        assert!(
            preview.contains("notes.txt") && preview.contains("AFTER"),
            "{preview}"
        );
        assert_eq!(std::fs::read(project.join("format-count")).unwrap(), b"xx");
        assert!(!project.join("post-hook.json").exists());
        assert_eq!(
            std::fs::read_to_string(project.join("notes.txt")).unwrap(),
            "before"
        );
        editor.answer_permission(
            &final_first,
            json!({"outcome":{"outcome":"selected","optionId":"allow-once"}}),
        );
        let final_second = editor.until_permission();
        let preview = final_second["params"]["toolCall"]["content"][0]["content"]["text"]
            .as_str()
            .unwrap();
        assert!(
            preview.contains("other.txt") && preview.contains("CHANGED"),
            "{preview}"
        );
        if scenario == "denied-changed-config" {
            std::fs::write(
                project.join(".rapidlm/settings.json"),
                json!({
                    "formatters": [{"command":["sh","-c","cat"],"globs":["*.txt"]}]
                })
                .to_string(),
            )
            .unwrap();
        }
        let checkpoint_path = std::fs::read_dir(project.join(".rapidlm/multi-edit-journal"))
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| entry.file_name().to_string_lossy().starts_with("format-"))
            .unwrap()
            .path();
        let pending_checkpoint = std::fs::read(&checkpoint_path).unwrap();
        editor.answer_permission(&final_second, json!({"outcome":{"outcome":"selected","optionId":if scenario == "approved" {"allow-once"} else {"reject-once"}}}));
        let frames = editor.play(3);
        assert_eq!(
            frames.last().unwrap()["result"]["stopReason"],
            "end_turn",
            "{frames:?}"
        );
        assert_eq!(std::fs::read(project.join("format-count")).unwrap(), b"xx");
        if scenario == "approved" {
            let post: Value =
                serde_json::from_slice(&std::fs::read(project.join("post-hook.json")).unwrap())
                    .unwrap();
            assert_eq!(post["tool"], "workspace_multi_patch");
        } else {
            assert!(!project.join("post-hook.json").exists());
        }
        assert_eq!(
            std::fs::read_to_string(project.join("notes.txt")).unwrap(),
            if scenario == "approved" {
                "AFTER"
            } else {
                "before"
            }
        );
        assert_eq!(
            std::fs::read_to_string(project.join("other.txt")).unwrap(),
            if scenario == "approved" {
                "CHANGED"
            } else {
                "second"
            }
        );
        let checkpoints: Vec<_> = std::fs::read_dir(project.join(".rapidlm/multi-edit-journal"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("format-"))
            .collect();
        assert_eq!(checkpoints.len(), 1, "{scenario}");
        assert_eq!(
            std::fs::read(checkpoints[0].path()).unwrap(),
            b"rapidlm.multi-format.retired.v1\n",
            "{scenario}"
        );
        assert_eq!(editor.finish(), Some(0));
        if scenario == "denied-changed-config" {
            // Recreate the durable state at the crash boundary immediately
            // after denial but before its cleanup observer.
            std::fs::write(&checkpoint_path, pending_checkpoint).unwrap();
            let mut reopened = Editor::spawn(&project, &home.0, &config);
            reopened.request(1, "initialize", json!({"protocolVersion": 1}));
            assert!(reopened.until_result(1).get("result").is_some());
            reopened.request(
                2,
                "session/load",
                json!({"sessionId":session, "cwd":project.display().to_string(), "mcpServers":[]}),
            );
            let loaded = reopened.until_result(2);
            assert!(loaded.get("result").is_some(), "{loaded}");
            assert_eq!(
                std::fs::read(&checkpoint_path).unwrap(),
                b"rapidlm.multi-format.retired.v1\n"
            );
            assert_eq!(std::fs::read(project.join("format-count")).unwrap(), b"xx");
            assert_eq!(reopened.finish(), Some(0));
        }
    }
}

#[cfg(unix)]
#[test]
fn acp_multi_file_shadow_checks_the_combined_formatted_candidate() {
    for scenario in ["passed", "failed"] {
        let home = temp_dir(&format!("multi-shadow-acp-{scenario}"));
        let (project, config) = trusted_project(&home.0, read_two_then_multi_file_patch);
        std::fs::write(project.join("notes.txt"), "before").unwrap();
        std::fs::write(project.join("other.txt"), "second").unwrap();
        for args in [
            vec!["add", "notes.txt", "other.txt"],
            vec![
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "commit",
                "-qm",
                "seed",
            ],
        ] {
            let output = Command::new("git")
                .args(args)
                .current_dir(&project)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let probe = project.join("shadow-probe");
        let script = if scenario == "passed" {
            format!(
                "printf ran >> '{}'; test \"$(cat notes.txt)\" = AFTER && test \"$(cat other.txt)\" = CHANGED",
                probe.display()
            )
        } else {
            format!("printf ran >> '{}'; exit 1", probe.display())
        };
        std::fs::write(
            project.join(".rapidlm/settings.json"),
            json!({
                "formatters":[{"command":["sh","-c","tr a-z A-Z"],"globs":["*.txt"]}],
                "shadow_diagnostics":{"command":["sh", "-c", script], "globs":["*.txt"]}
            })
            .to_string(),
        )
        .unwrap();
        let mut editor = Editor::spawn(&project, &home.0, &config);
        let session = editor.open_session(&project);
        editor.start_prompt(3, &session, "patch both files");
        for _ in 0..4 {
            let request = editor.until_permission();
            assert!(!probe.exists(), "shadow checks ran before all approvals");
            editor.answer_permission(
                &request,
                json!({"outcome":{"outcome":"selected","optionId":"allow-once"}}),
            );
        }
        let frames = editor.play(3);
        assert_eq!(
            frames.last().unwrap()["result"]["stopReason"],
            "end_turn",
            "{frames:?}"
        );
        assert_eq!(
            std::fs::read(&probe).unwrap(),
            if scenario == "passed" {
                b"ranran".as_slice()
            } else {
                b"ran".as_slice()
            }
        );
        let checkpoints: Vec<_> = std::fs::read_dir(project.join(".rapidlm/multi-edit-journal"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("format-"))
            .collect();
        assert_eq!(checkpoints.len(), 1, "{scenario}");
        assert_eq!(
            std::fs::read(checkpoints[0].path()).unwrap(),
            b"rapidlm.multi-format.retired.v1\n",
            "{scenario}"
        );
        assert_eq!(
            std::fs::read_to_string(project.join("notes.txt")).unwrap(),
            if scenario == "passed" {
                "AFTER"
            } else {
                "before"
            }
        );
        assert_eq!(
            std::fs::read_to_string(project.join("other.txt")).unwrap(),
            if scenario == "passed" {
                "CHANGED"
            } else {
                "second"
            }
        );
        assert_eq!(editor.finish(), Some(0));
    }
}

fn read_then_large_multi_patch(request: &str) -> String {
    match tool_results(request) {
        0 => tool_call("read-large", "repo_read", json!({"path":"notes.txt"})),
        1 => {
            let edits: Vec<Value> = (0..4).map(|i| json!({"old":format!("old-{i}-{}", "x".repeat(1200)), "new":format!("new-{i}-{}", "y".repeat(1200))})).collect();
            tool_call(
                "patch-large",
                "workspace_patch",
                json!({"path":"notes.txt", "edits":edits}),
            )
        }
        _ => answer("done"),
    }
}

#[test]
fn acp_approval_preview_explicitly_marks_large_multi_edit_as_truncated() {
    let home = temp_dir("multi-patch-preview-bound");
    let (project, config) = trusted_project(&home.0, read_then_large_multi_patch);
    let original = (0..4)
        .map(|i| format!("old-{i}-{}\n", "x".repeat(1200)))
        .collect::<String>();
    std::fs::write(project.join("notes.txt"), &original).unwrap();
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);
    editor.start_prompt(3, &session, "patch notes");
    let permission = editor.until_permission();
    let preview = permission["params"]["toolCall"]["content"][0]["content"]["text"]
        .as_str()
        .expect("preview");
    assert!(preview.contains("truncated"), "{preview}");
    editor.answer_permission(
        &permission,
        json!({"outcome":{"outcome":"selected","optionId":"reject-once"}}),
    );
    editor.play(3);
    assert_eq!(
        std::fs::read_to_string(project.join("notes.txt")).unwrap(),
        original
    );
    assert_eq!(editor.finish(), Some(0));
}

#[test]
fn acp_multi_patch_approval_preserves_all_edits_and_rechecks_read() {
    for scenario in ["fresh", "changed", "denied"] {
        let home = temp_dir("multi-patch-approval");
        let (project, config) = trusted_project(&home.0, read_then_multi_patch);
        std::fs::write(project.join("notes.txt"), "before").unwrap();
        let mut editor = Editor::spawn(&project, &home.0, &config);
        let session = editor.open_session(&project);
        editor.start_prompt(3, &session, "patch notes");
        let permission = editor.until_permission();
        assert_eq!(
            permission["params"]["toolCall"]["toolCallId"],
            "patch-multi"
        );
        let preview = permission["params"]["toolCall"]["content"][0]["content"]["text"]
            .as_str()
            .expect("approval preview");
        assert!(
            preview.contains("Edit 1:")
                && preview.contains("before")
                && preview.contains("middle")
                && preview.contains("Edit 2:")
                && preview.contains("after"),
            "{preview}"
        );
        if scenario == "changed" {
            std::fs::write(project.join("notes.txt"), "external").unwrap();
        }
        let option = if scenario == "denied" {
            "reject-once"
        } else {
            "allow-once"
        };
        editor.answer_permission(
            &permission,
            json!({"outcome":{"outcome":"selected","optionId":option}}),
        );
        let frames = editor.play(3);
        assert_eq!(
            frames.last().unwrap()["result"]["stopReason"],
            "end_turn",
            "{scenario}: {frames:?}"
        );
        assert_eq!(
            std::fs::read_to_string(project.join("notes.txt")).unwrap(),
            match scenario {
                "fresh" => "after",
                "changed" => "external",
                _ => "before",
            },
            "{scenario}"
        );
        assert_eq!(editor.finish(), Some(0));
    }
}

fn cross_turn_read_then_edit(request: &str) -> String {
    if request.contains("EDIT_NO_READ") {
        return if tool_results(request) == 0 {
            tool_call(
                "write-other-session",
                "workspace_write",
                json!({"path":"notes.txt","content":"after"}),
            )
        } else {
            answer("other session finished")
        };
    }
    if request.contains("EDIT_PHASE") {
        if !request.contains("session-read/") || !request.contains("before") {
            return answer("REPLAY_MISSING");
        }
        if tool_results(request) == 0 {
            return tool_call(
                "write-cross-turn",
                "workspace_write",
                json!({"path":"notes.txt","content":"after"}),
            );
        }
        return answer("edit finished");
    }
    if tool_results(request) == 0 {
        tool_call("read-cross-turn", "repo_read", json!({"path":"notes.txt"}))
    } else {
        answer("read finished")
    }
}

#[test]
fn acp_cross_turn_read_proofs_do_not_cross_sessions() {
    let home = temp_dir("cross-session-read");
    let (project, config) = trusted_project(&home.0, cross_turn_read_then_edit);
    std::fs::write(project.join("notes.txt"), "before").unwrap();
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let first_session = editor.open_session(&project);
    let first = editor.prompt(3, &first_session, "READ_PHASE read notes");
    assert_eq!(first.last().unwrap()["result"]["stopReason"], "end_turn");
    editor.request(
        4,
        "session/new",
        json!({"cwd":project.display().to_string(),"mcpServers":[]}),
    );
    let second_session = editor.until_result(4)["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_owned();
    let second = editor.prompt(5, &second_session, "EDIT_NO_READ edit notes");
    assert_eq!(
        second.last().unwrap()["result"]["stopReason"],
        "end_turn",
        "{second:?}"
    );
    assert!(
        format!("{second:?}").contains("write-other-session"),
        "{second:?}"
    );
    assert_eq!(
        std::fs::read_to_string(project.join("notes.txt")).unwrap(),
        "before",
        "{second:?}"
    );
    assert_eq!(editor.finish(), Some(0));
}

fn restart_read_then_edit(request: &str) -> String {
    let phase = ["READ_PHASE", "EDIT_NO_READ", "EDIT_PHASE"]
        .into_iter()
        .filter_map(|marker| request.find(marker).map(|at| (at, marker)))
        .min_by_key(|(at, _)| *at)
        .map(|(_, marker)| marker)
        .unwrap_or("");
    match (phase, tool_results(request)) {
        ("READ_PHASE", 0) => tool_call("read-restart", "repo_read", json!({"path":"notes.txt"})),
        ("EDIT_NO_READ", 0) => tool_call(
            "write-after-restart",
            "workspace_write",
            json!({"path":"notes.txt","content":"after"}),
        ),
        ("EDIT_PHASE", 0) if request.contains("session-read/") && request.contains("before") => {
            tool_call(
                "write-after-reread",
                "workspace_write",
                json!({"path":"notes.txt","content":"after"}),
            )
        }
        ("EDIT_PHASE", 0) => answer("REPLAY_MISSING"),
        _ => answer("done"),
    }
}

#[test]
fn acp_restart_does_not_reconstruct_read_authority_from_session_history() {
    let home = temp_dir("restart-read-authority");
    let (project, config) = trusted_project(&home.0, restart_read_then_edit);
    std::fs::write(project.join("notes.txt"), "before").unwrap();
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);
    let first = editor.prompt(3, &session, "READ_PHASE read notes");
    assert_eq!(first.last().unwrap()["result"]["stopReason"], "end_turn");
    assert!(
        first.iter().any(|frame| {
            frame["params"]["update"]["toolCallId"] == "read-restart"
                && frame["params"]["update"]["status"] == "completed"
        }),
        "{first:?}"
    );
    assert_eq!(editor.finish(), Some(0));

    let mut reopened = Editor::spawn(&project, &home.0, &config);
    reopened.request(1, "initialize", json!({"protocolVersion":1}));
    assert!(reopened.until_result(1).get("result").is_some());
    reopened.request(
        2,
        "session/load",
        json!({"sessionId":session,"cwd":project.display().to_string(),"mcpServers":[]}),
    );
    let loaded = reopened.until_result(2);
    assert!(loaded.get("result").is_some(), "{loaded}");
    reopened.request(
        3,
        "session/set_mode",
        json!({"sessionId":session,"mode":"acceptEdits"}),
    );
    let mode = reopened.until_result(3);
    assert!(mode.get("result").is_some(), "{mode}");
    let refused = reopened.prompt(4, &session, "EDIT_NO_READ edit notes");
    assert!(
        refused.iter().any(|frame| {
            frame["params"]["update"]["toolCallId"] == "write-after-restart"
                && frame["params"]["update"]["status"] == "failed"
        }),
        "{refused:?}"
    );
    assert_eq!(
        std::fs::read_to_string(project.join("notes.txt")).unwrap(),
        "before"
    );

    let reread = reopened.prompt(5, &session, "READ_PHASE read notes again");
    assert_eq!(reread.last().unwrap()["result"]["stopReason"], "end_turn");
    let edit = reopened.prompt(6, &session, "EDIT_PHASE edit notes");
    assert!(!format!("{edit:?}").contains("REPLAY_MISSING"), "{edit:?}");
    assert_eq!(
        std::fs::read_to_string(project.join("notes.txt")).unwrap(),
        "after"
    );
    assert_eq!(reopened.finish(), Some(0));
}

#[test]
fn acp_cross_turn_edits_require_a_replayed_fresh_read_on_the_provider_wire() {
    for scenario in ["fresh", "changed"] {
        let home = temp_dir("cross-turn-read");
        let (project, config) = trusted_project(&home.0, cross_turn_read_then_edit);
        std::fs::write(project.join("notes.txt"), "before").unwrap();
        let mut editor = Editor::spawn(&project, &home.0, &config);
        let session = editor.open_session(&project);
        let first = editor.prompt(3, &session, "READ_PHASE read notes");
        assert_eq!(
            first.last().unwrap()["result"]["stopReason"],
            "end_turn",
            "{first:?}"
        );
        if scenario == "changed" {
            std::fs::write(project.join("notes.txt"), "external").unwrap();
        }
        let second = editor.prompt(4, &session, "EDIT_PHASE edit notes");
        assert_eq!(
            second.last().unwrap()["result"]["stopReason"],
            "end_turn",
            "{second:?}"
        );
        assert!(
            !format!("{second:?}").contains("REPLAY_MISSING"),
            "{second:?}"
        );
        assert_eq!(
            std::fs::read_to_string(project.join("notes.txt")).unwrap(),
            if scenario == "fresh" {
                "after"
            } else {
                "external"
            },
            "{scenario}: {second:?}"
        );
        assert_eq!(editor.finish(), Some(0));
    }
}

static ITEM_DELEGATION_REPORTED: AtomicBool = AtomicBool::new(false);

fn item_delegation_model(request: &str) -> String {
    if tool_results(request) > 0 {
        if request.contains("item 1 [reported]")
            && request.contains("item 2 [reported]")
            && request.contains("CHILD_ALPHA")
            && request.contains("CHILD_BETA")
        {
            ITEM_DELEGATION_REPORTED.store(true, Ordering::SeqCst);
            return answer("FANOUT_OK");
        }
        return answer("FANOUT_MISSING_REPORT");
    }
    if request.contains("child-task ALPHA") {
        return answer("CHILD_ALPHA");
    }
    if request.contains("child-task BETA") {
        return answer("CHILD_BETA");
    }
    tool_call(
        "spawn-items",
        "task_spawn",
        json!({
            "prompt_template":"child-task {{item}}",
            "items":["ALPHA","BETA"],
            "type":"explore"
        }),
    )
}

#[test]
fn acp_item_delegation_runs_real_children_and_returns_both_reports_to_model() {
    ITEM_DELEGATION_REPORTED.store(false, Ordering::SeqCst);
    let home = temp_dir("item-delegation");
    let (project, config) = trusted_project(&home.0, item_delegation_model);
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);
    editor.request(
        3,
        "session/set_mode",
        json!({"sessionId":session,"mode":"bypassPermissions"}),
    );
    let mode = editor.until_result(3);
    assert!(mode.get("result").is_some(), "{mode}");
    let frames = editor.prompt(4, &session, "delegate the two items");
    assert!(
        ITEM_DELEGATION_REPORTED.load(Ordering::SeqCst),
        "{frames:?}"
    );
    assert_eq!(editor.finish(), Some(0));
}

fn item_identity_model(request: &str) -> String {
    let body = request
        .split_once("\r\n\r\n")
        .map_or(request, |(_, body)| body);
    let parsed: Value = serde_json::from_str(body).unwrap();
    let messages = parsed["messages"].as_array().unwrap();
    let is_child = messages.iter().any(|message| {
        message["role"] == "user"
            && message["content"]
                .as_str()
                .is_some_and(|text| text.contains("IDENTITY_CHILD"))
    });
    if is_child {
        return if tool_results(request) == 0 {
            tool_call("identity-glob", "repo_glob", json!({"pattern":"*.txt"}))
        } else {
            answer("child done")
        };
    }
    if tool_results(request) > 0 {
        return answer("parent done");
    }
    tool_call(
        "identity-items",
        "task_spawn",
        json!({
            "prompt_template":"IDENTITY_CHILD {{item}}","items":["a","b"],"type":"explore"
        }),
    )
}

#[cfg(unix)]
#[test]
fn acp_fanout_child_hooks_keep_the_parent_session_and_turn_identity() {
    let home = temp_dir("item-identities");
    let (project, config) = trusted_project(&home.0, item_identity_model);
    std::fs::write(
        project.join("start-hook.sh"),
        "printf 'start %s %s\\n' \"$RAPIDLM_SESSION_ID\" \"$RAPIDLM_TURN_ID\" >> identity.log\n",
    )
    .unwrap();
    std::fs::write(
        project.join("pre-hook.sh"),
        "printf 'pre %s %s\\n' \"$RAPIDLM_SESSION_ID\" \"$RAPIDLM_TURN_ID\" >> identity.log\n",
    )
    .unwrap();
    std::fs::write(
        project.join(".rapidlm/settings.json"),
        json!({"hooks":{
            "subagent_start":[{"command":"sh start-hook.sh"}],
            "pre_tool_use":[{"command":"sh pre-hook.sh","matcher":"repo_glob"}]
        }})
        .to_string(),
    )
    .unwrap();
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);
    editor.request(
        3,
        "session/set_mode",
        json!({"sessionId":session,"mode":"bypassPermissions"}),
    );
    assert!(editor.until_result(3).get("result").is_some());
    let frames = editor.prompt(4, &session, "run identity fanout");
    assert_eq!(
        frames.last().unwrap()["result"]["stopReason"],
        "end_turn",
        "{frames:?}"
    );
    let contents = std::fs::read_to_string(project.join("identity.log")).unwrap();
    let lines: Vec<_> = contents
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>())
        .collect();
    assert_eq!(lines.len(), 4, "{contents}");
    assert_eq!(
        lines.iter().filter(|line| line[0] == "start").count(),
        2,
        "{contents}"
    );
    assert_eq!(
        lines.iter().filter(|line| line[0] == "pre").count(),
        2,
        "{contents}"
    );
    let mut turns = std::collections::HashSet::new();
    for line in lines {
        assert_eq!(line.len(), 3, "{contents}");
        assert_eq!(line[1], session, "{contents}");
        line[2].parse::<protocol::TurnId>().expect("turn id");
        turns.insert(line[2].to_owned());
    }
    assert_eq!(turns.len(), 1, "{contents}");
    assert_eq!(editor.finish(), Some(0));
}

static ITEM_REPORT_READBACK_SEEN: AtomicBool = AtomicBool::new(false);

fn item_report_readback_model(request: &str) -> String {
    let body = request
        .split_once("\r\n\r\n")
        .map_or(request, |(_, body)| body);
    let parsed: Value = serde_json::from_str(body).unwrap();
    let messages = parsed["messages"].as_array().unwrap();
    let parent = messages.iter().any(|message| {
        message["role"] == "user"
            && message["content"]
                .as_str()
                .is_some_and(|text| text.contains("READBACK_PARENT"))
    });
    if !parent {
        return answer(&format!("{} REPORT_TAIL_MARKER", "x".repeat(9000)));
    }
    let tool_messages: Vec<_> = messages
        .iter()
        .filter(|message| message["role"] == "tool")
        .collect();
    if tool_messages.is_empty() {
        return tool_call(
            "report-spawn",
            "task_spawn",
            json!({
                "prompt_template":"child {{item}}","items":["one","two"],"type":"explore"
            }),
        );
    }
    let group = tool_messages[0]["content"].as_str().unwrap();
    let marker = "task_report(report_id=\"";
    let start = group.find(marker).unwrap() + marker.len();
    let id = &group[start..start + 36];
    if request.contains("REPORT_TAIL_MARKER") {
        ITEM_REPORT_READBACK_SEEN.store(true, Ordering::SeqCst);
        return answer("readback complete");
    }
    let offset = if tool_messages.len() == 1 {
        0
    } else {
        let last = tool_messages.last().unwrap()["content"].as_str().unwrap();
        let start = last.find("next offset ").unwrap() + "next offset ".len();
        last[start..]
            .split(':')
            .next()
            .unwrap()
            .parse::<usize>()
            .unwrap()
    };
    tool_call(
        &format!("report-page-{offset}"),
        "task_report",
        json!({"report_id":id,"offset":offset}),
    )
}

#[test]
fn acp_fanout_report_readback_reaches_the_parent_model() {
    ITEM_REPORT_READBACK_SEEN.store(false, Ordering::SeqCst);
    let home = temp_dir("item-report-readback");
    let (project, config) = trusted_project(&home.0, item_report_readback_model);
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);
    editor.request(
        3,
        "session/set_mode",
        json!({"sessionId":session,"mode":"bypassPermissions"}),
    );
    assert!(editor.until_result(3).get("result").is_some());
    let frames = editor.prompt(4, &session, "READBACK_PARENT");
    assert!(
        ITEM_REPORT_READBACK_SEEN.load(Ordering::SeqCst),
        "{frames:?}"
    );
    assert_eq!(editor.finish(), Some(0));
}

static CROSS_TURN_REPORT_ID: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
static CROSS_TURN_REPORT_TAIL: AtomicBool = AtomicBool::new(false);

fn cross_turn_report_model(request: &str) -> String {
    let body = request
        .split_once("\r\n\r\n")
        .map_or(request, |(_, body)| body);
    let parsed: Value = serde_json::from_str(body).unwrap();
    let messages = parsed["messages"].as_array().unwrap();
    let current = messages
        .iter()
        .filter(|message| message["role"] == "user")
        .filter_map(|message| message["content"].as_str())
        .find(|text| {
            text.contains("STORE_REPORT_PARENT")
                || text.contains("FETCH_REPORT_PARENT")
                || text.contains("FETCH_AFTER_RESTART")
        });
    let Some(current) = current else {
        return answer(&format!("{} REPORT_PERSIST_TAIL", "z".repeat(9000)));
    };
    let tool_messages: Vec<_> = messages
        .iter()
        .filter(|message| message["role"] == "tool")
        .collect();
    if current.contains("STORE_REPORT_PARENT") {
        if tool_messages.is_empty() {
            return tool_call(
                "store-report",
                "task_spawn",
                json!({
                    "prompt_template":"report child {{item}}","items":["one","two"],"type":"explore"
                }),
            );
        }
        let group = tool_messages[0]["content"].as_str().unwrap();
        let marker = "task_report(report_id=\"";
        let start = group.find(marker).unwrap() + marker.len();
        *CROSS_TURN_REPORT_ID.lock().unwrap() = Some(group[start..start + 36].to_owned());
        return answer("stored");
    }
    let id = CROSS_TURN_REPORT_ID.lock().unwrap().clone().unwrap();
    if current.contains("FETCH_AFTER_RESTART") {
        return tool_call(
            "lost-report",
            "task_report",
            json!({"report_id":id,"offset":0}),
        );
    }
    if request.contains("REPORT_PERSIST_TAIL") {
        CROSS_TURN_REPORT_TAIL.store(true, Ordering::SeqCst);
        return answer("fetched");
    }
    let offset = if tool_messages.is_empty() {
        0
    } else {
        let last = tool_messages.last().unwrap()["content"].as_str().unwrap();
        let start = last.find("next offset ").unwrap() + "next offset ".len();
        last[start..]
            .split(':')
            .next()
            .unwrap()
            .parse::<usize>()
            .unwrap()
    };
    tool_call(
        &format!("fetch-page-{offset}"),
        "task_report",
        json!({"report_id":id,"offset":offset}),
    )
}

#[test]
fn acp_report_readback_is_retained_per_session_until_host_restart() {
    *CROSS_TURN_REPORT_ID.lock().unwrap() = None;
    CROSS_TURN_REPORT_TAIL.store(false, Ordering::SeqCst);
    let home = temp_dir("cross-turn-report");
    let (project, config) = trusted_project(&home.0, cross_turn_report_model);
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);
    editor.request(
        3,
        "session/set_mode",
        json!({"sessionId":session,"mode":"bypassPermissions"}),
    );
    assert!(editor.until_result(3).get("result").is_some());
    let first = editor.prompt(4, &session, "STORE_REPORT_PARENT");
    assert!(CROSS_TURN_REPORT_ID.lock().unwrap().is_some(), "{first:?}");
    editor.request(
        5,
        "session/new",
        json!({"cwd":project.display().to_string(),"mcpServers":[]}),
    );
    let other = editor.until_result(5)["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_owned();
    let denied = editor.prompt(6, &other, "FETCH_AFTER_RESTART");
    assert!(
        denied.iter().any(
            |frame| frame["params"]["update"]["toolCallId"] == "lost-report"
                && frame["params"]["update"]["status"] == "failed"
        ),
        "{denied:?}"
    );
    let fetched = editor.prompt(7, &session, "FETCH_REPORT_PARENT");
    assert!(CROSS_TURN_REPORT_TAIL.load(Ordering::SeqCst), "{fetched:?}");
    assert_eq!(editor.finish(), Some(0));
    let mut reopened = Editor::spawn(&project, &home.0, &config);
    reopened.request(1, "initialize", json!({"protocolVersion":1}));
    assert!(reopened.until_result(1).get("result").is_some());
    reopened.request(
        2,
        "session/load",
        json!({"sessionId":session,"cwd":project.display().to_string(),"mcpServers":[]}),
    );
    assert!(reopened.until_result(2).get("result").is_some());
    reopened.request(
        3,
        "session/set_mode",
        json!({"sessionId":session,"mode":"bypassPermissions"}),
    );
    assert!(reopened.until_result(3).get("result").is_some());
    let lost = reopened.prompt(4, &session, "FETCH_AFTER_RESTART");
    assert!(
        lost.iter().any(
            |frame| frame["params"]["update"]["toolCallId"] == "lost-report"
                && frame["params"]["update"]["status"] == "failed"
        ),
        "{lost:?}"
    );
    assert_eq!(reopened.finish(), Some(0));
}

static ITEM_RESUME_REPORTED: AtomicBool = AtomicBool::new(false);

fn item_resume_model(request: &str) -> String {
    if request.contains("CONTINUE_FIRST_CHILD") {
        if request.contains("continued_from=") {
            if request.contains("CHILD_RESUMED") {
                ITEM_RESUME_REPORTED.store(true, Ordering::SeqCst);
            }
            return answer("resume checked");
        }
        if request.contains("initial child report") && request.contains("child-task ") {
            return answer("CHILD_RESUMED");
        }
        return answer("RESUME_HISTORY_MISSING");
    }
    if tool_results(request) > 0 {
        let Some(start) = request.find("agent_id=") else {
            return answer("missing agent id");
        };
        let id = &request[start + "agent_id=".len()..start + "agent_id=".len() + 36];
        return tool_call(
            "resume-items",
            "task_spawn",
            json!({
                "resume_agent_ids": {id: "CONTINUE_FIRST_CHILD"}
            }),
        );
    }
    if request.contains("child-task ALPHA") || request.contains("child-task BETA") {
        return answer("initial child report");
    }
    tool_call(
        "spawn-items",
        "task_spawn",
        json!({
            "prompt_template":"child-task {{item}}",
            "items":["ALPHA","BETA"],
            "type":"explore"
        }),
    )
}

#[test]
fn acp_item_delegation_resumes_a_real_retained_child_with_its_history() {
    ITEM_RESUME_REPORTED.store(false, Ordering::SeqCst);
    let home = temp_dir("item-resume");
    let (project, config) = trusted_project(&home.0, item_resume_model);
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);
    editor.request(
        3,
        "session/set_mode",
        json!({"sessionId":session,"mode":"bypassPermissions"}),
    );
    assert!(editor.until_result(3).get("result").is_some());
    let frames = editor.prompt(4, &session, "delegate then continue one child");
    assert!(ITEM_RESUME_REPORTED.load(Ordering::SeqCst), "{frames:?}");
    assert_eq!(editor.finish(), Some(0));
}

static CROSS_TURN_CHILD_ID: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
static CROSS_TURN_CHILD_RESUMED: AtomicBool = AtomicBool::new(false);
static CROSS_TURN_POLICY_CHECK: AtomicBool = AtomicBool::new(false);
static CROSS_TURN_POLICY_DENIED: AtomicBool = AtomicBool::new(false);
static CROSS_TURN_SECRET_EXPOSED: AtomicBool = AtomicBool::new(false);

fn cross_turn_item_resume_model(request: &str) -> String {
    let body = request
        .split_once("\r\n\r\n")
        .map_or(request, |(_, body)| body);
    let parsed: Value = serde_json::from_str(body).unwrap();
    let current = parsed["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "user")
        .filter_map(|message| message["content"].as_str())
        .find(|text| text.contains("BEGIN_PARENT") || text.contains("CONTINUE_PARENT"));
    if let Some(current) = current {
        if current.contains("CONTINUE_PARENT") {
            if request.contains("continued_from=") && request.contains("FOLLOWUP_RESULT") {
                CROSS_TURN_CHILD_RESUMED.store(true, Ordering::SeqCst);
                return answer("continued");
            }
            let id = CROSS_TURN_CHILD_ID.lock().unwrap().clone().unwrap();
            return tool_call(
                "cross-turn-resume",
                "task_spawn",
                json!({
                    "resume_agent_ids": {id: "FOLLOWUP_NEXT"}
                }),
            );
        }
        if tool_results(request) > 0 {
            let start = request.find("agent_id=").unwrap() + "agent_id=".len();
            *CROSS_TURN_CHILD_ID.lock().unwrap() = Some(request[start..start + 36].to_owned());
            return answer("spawned");
        }
        return tool_call(
            "cross-turn-spawn",
            "task_spawn",
            json!({
                "prompt_template":"ORIGINAL_CHILD {{item}}",
                "items":["ALPHA","BETA"],
                "type":"explore"
            }),
        );
    }
    if request.contains("FOLLOWUP_NEXT") {
        if CROSS_TURN_POLICY_CHECK.load(Ordering::SeqCst) {
            if request.contains("DO_NOT_EXPOSE_SECRET") {
                CROSS_TURN_SECRET_EXPOSED.store(true, Ordering::SeqCst);
            }
            if request.contains("policy-read") {
                if request.to_ascii_lowercase().contains("denied") {
                    CROSS_TURN_POLICY_DENIED.store(true, Ordering::SeqCst);
                }
                return answer("FOLLOWUP_RESULT");
            }
            return tool_call("policy-read", "repo_read", json!({"path":"private.txt"}));
        }
        if request.contains("ORIGINAL_CHILD") && request.contains("ORIGINAL_REPORT") {
            return answer("FOLLOWUP_RESULT");
        }
        return answer("HISTORY_MISSING");
    }
    answer("ORIGINAL_REPORT")
}

#[test]
fn acp_retains_child_continuations_across_turns_in_the_same_session() {
    *CROSS_TURN_CHILD_ID.lock().unwrap() = None;
    CROSS_TURN_CHILD_RESUMED.store(false, Ordering::SeqCst);
    CROSS_TURN_POLICY_CHECK.store(false, Ordering::SeqCst);
    CROSS_TURN_POLICY_DENIED.store(false, Ordering::SeqCst);
    CROSS_TURN_SECRET_EXPOSED.store(false, Ordering::SeqCst);
    let home = temp_dir("cross-turn-child-resume");
    let (project, config) = trusted_project(&home.0, cross_turn_item_resume_model);
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);
    editor.request(
        3,
        "session/set_mode",
        json!({"sessionId":session,"mode":"bypassPermissions"}),
    );
    assert!(editor.until_result(3).get("result").is_some());
    let first = editor.prompt(4, &session, "BEGIN_PARENT");
    assert!(CROSS_TURN_CHILD_ID.lock().unwrap().is_some(), "{first:?}");
    editor.request(
        5,
        "session/new",
        json!({"cwd":project.display().to_string(),"mcpServers":[]}),
    );
    let other = editor.until_result(5)["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_owned();
    let refused = editor.prompt(6, &other, "CONTINUE_PARENT");
    assert!(
        refused.iter().any(
            |frame| frame["params"]["update"]["toolCallId"] == "cross-turn-resume"
                && frame["params"]["update"]["status"] == "failed"
        ),
        "{refused:?}"
    );
    assert!(!CROSS_TURN_CHILD_RESUMED.load(Ordering::SeqCst));
    let original_config = std::fs::read_to_string(&config).unwrap();
    std::fs::write(
        &config,
        original_config.replace("test-model", "changed-model"),
    )
    .unwrap();
    let changed = editor.prompt(7, &session, "CONTINUE_PARENT");
    assert!(
        changed.iter().any(
            |frame| frame["params"]["update"]["toolCallId"] == "cross-turn-resume"
                && frame["params"]["update"]["status"] == "failed"
        ),
        "{changed:?}"
    );
    assert!(!CROSS_TURN_CHILD_RESUMED.load(Ordering::SeqCst));
    std::fs::write(&config, original_config).unwrap();
    std::fs::write(project.join("private.txt"), "DO_NOT_EXPOSE_SECRET").unwrap();
    std::fs::write(
        project.join(".rapidlm/settings.json"),
        json!({"permissions":{"deny":["repo_read(private.txt)"]}}).to_string(),
    )
    .unwrap();
    CROSS_TURN_POLICY_CHECK.store(true, Ordering::SeqCst);
    let second = editor.prompt(8, &session, "CONTINUE_PARENT");
    assert!(
        CROSS_TURN_CHILD_RESUMED.load(Ordering::SeqCst),
        "{second:?}"
    );
    assert!(
        CROSS_TURN_POLICY_DENIED.load(Ordering::SeqCst),
        "{second:?}"
    );
    assert!(!CROSS_TURN_SECRET_EXPOSED.load(Ordering::SeqCst));
    assert_eq!(editor.finish(), Some(0));
}

static DETACHED_CONTINUATION: std::sync::Mutex<Option<(String, String)>> =
    std::sync::Mutex::new(None);
static DETACHED_CONTINUED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
static PLUGIN_SKILL_SEEN: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
fn plugin_skill_model(request: &str) -> String {
    if tool_results(request) > 0 {
        if request.contains("BUNDLE_MODEL_INSTRUCTIONS")
            && request.contains("BUNDLE_MODEL_RESOURCE")
        {
            PLUGIN_SKILL_SEEN.store(true, Ordering::SeqCst);
        }
        return answer("skill checked");
    }
    tool_call("activate-bundle", "skill", json!({"name":"bundlefmt"}))
}

#[cfg(unix)]
#[test]
fn acp_model_activates_an_approved_plugin_skill_with_its_resource() {
    PLUGIN_SKILL_SEEN.store(false, Ordering::SeqCst);
    let home = temp_dir("acp-plugin-skill");
    let (project, config) = trusted_project(&home.0, plugin_skill_model);
    let bundle = json!({"schema":"rapidlm.plugin_bundle","schema_version":1,"skills":[{"id":"bundlefmt","skill_md":"---\nname: bundlefmt\ndescription: Format sources\nresources:\n  - guide.md\n---\nBUNDLE_MODEL_INSTRUCTIONS\n","resources":[{"path":"guide.md","text":"BUNDLE_MODEL_RESOURCE"}]}]});
    let mut bytes = plugin_host::bundle::BUNDLE_MAGIC.to_vec();
    bytes.extend(serde_json::to_vec(&bundle).unwrap());
    let manifest = json!({"schema":"rapidlm.plugin_manifest","schema_version":1,"id":"acme.fmt","version":"1.0.0","publisher":"acme","digest":protocol::ArtifactId::from_bytes(&bytes).to_string(),"entrypoint":"plugin.wasm","wit_version":"1.0.0","compatibility":{"min":"1.0.0","max":"2.0.0"},"requested_caps":[],"skills":["bundlefmt"]});
    let manifest_path = home.0.join("manifest.json");
    let package_path = home.0.join("package.bin");
    std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    std::fs::write(&package_path, bytes).unwrap();
    let installed = rapid(&project, &home.0, &config)
        .args([
            "plugins",
            "install",
            manifest_path.to_str().unwrap(),
            package_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let approved = rapid(&project, &home.0, &config)
        .args(["plugins", "approve", "acme.fmt", "--content"])
        .output()
        .unwrap();
    assert!(
        approved.status.success(),
        "{}",
        String::from_utf8_lossy(&approved.stderr)
    );
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);
    editor.request(
        3,
        "session/set_mode",
        json!({"sessionId":session,"mode":"bypassPermissions"}),
    );
    assert!(editor.until_result(3).get("result").is_some());
    let frames = editor.prompt(4, &session, "Use the formatter skill");
    assert!(PLUGIN_SKILL_SEEN.load(Ordering::SeqCst), "{frames:?}");
    assert_eq!(editor.finish(), Some(0));
}

fn detached_continuation_model(request: &str) -> String {
    let body = request
        .split_once("\r\n\r\n")
        .map_or(request, |(_, body)| body);
    let parsed: Value = serde_json::from_str(body).unwrap();
    let parent = parsed["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .filter_map(|m| m["content"].as_str())
        .rfind(|text| *text == "BG_PARENT_START" || *text == "BG_PARENT_CONTINUE");
    if let Some(parent) = parent {
        if parent.contains("BG_PARENT_CONTINUE") {
            if request.contains("bg-continue") {
                if request.contains("BG_CHILD_RESUMED") {
                    DETACHED_CONTINUED.store(true, Ordering::SeqCst);
                }
                return answer("background continuation complete");
            }
            let (agent, job) = DETACHED_CONTINUATION.lock().unwrap().clone().unwrap();
            if request.contains("bg-wait") {
                return tool_call(
                    "bg-continue",
                    "task_spawn",
                    json!({"resume_agent_ids": {agent: "BG_FOLLOWUP"}}),
                );
            }
            return tool_call(
                "bg-wait",
                "job_status",
                json!({"job_id":job,"wait_ms":5000}),
            );
        }
        if tool_results(request) > 0 {
            if !request.contains("agent=") || !request.contains("job=") {
                return answer("background spawn failed");
            }
            let agent_start = request.find("agent=").unwrap() + "agent=".len();
            let agent = request[agent_start..agent_start + 36].to_owned();
            let job_start = request.find("job=").unwrap() + "job=".len();
            let job = request[job_start..]
                .split_whitespace()
                .next()
                .unwrap()
                .to_owned();
            *DETACHED_CONTINUATION.lock().unwrap() = Some((agent, job));
            return answer("background child started");
        }
        return tool_call(
            "bg-spawn",
            "task_spawn",
            json!({"prompt":"BG_CHILD_ORIGINAL","type":"explore","background":true}),
        );
    }
    if request.contains("BG_FOLLOWUP")
        && request.contains("BG_CHILD_ORIGINAL")
        && request.contains("BG_CHILD_REPORT")
    {
        return answer("BG_CHILD_RESUMED");
    }
    answer("BG_CHILD_REPORT")
}

#[test]
fn acp_continues_a_completed_background_child_in_a_later_turn() {
    *DETACHED_CONTINUATION.lock().unwrap() = None;
    DETACHED_CONTINUED.store(false, Ordering::SeqCst);
    let home = temp_dir("background-child-continuation");
    let (project, config) = trusted_project(&home.0, detached_continuation_model);
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);
    editor.request(
        3,
        "session/set_mode",
        json!({"sessionId":session,"mode":"bypassPermissions"}),
    );
    assert!(editor.until_result(3).get("result").is_some());
    let first = editor.prompt(4, &session, "BG_PARENT_START");
    assert!(DETACHED_CONTINUATION.lock().unwrap().is_some(), "{first:?}");
    let second = editor.prompt(5, &session, "BG_PARENT_CONTINUE");
    assert!(DETACHED_CONTINUED.load(Ordering::SeqCst), "{second:?}");
    assert_eq!(editor.finish(), Some(0));
}

#[test]
fn acp_approved_edit_reuses_only_a_fresh_visible_read() {
    for scenario in ["fresh", "changed", "plan"] {
        let home = temp_dir("visible-read");
        let (project, config) = trusted_project(&home.0, read_then_approved_edit);
        std::fs::write(project.join("notes.txt"), "before").unwrap();
        let mut editor = Editor::spawn(&project, &home.0, &config);
        let session = editor.open_session(&project);
        editor.start_prompt(3, &session, "read then edit notes");
        let permission = editor.until_permission();
        assert_eq!(permission["params"]["toolCall"]["toolCallId"], "write-1");
        if scenario == "changed" {
            std::fs::write(project.join("notes.txt"), "external").unwrap();
        }
        if scenario == "plan" {
            editor.request(
                4,
                "session/set_mode",
                json!({"sessionId":session,"mode":"plan"}),
            );
            let changed = editor.until_result(4);
            assert!(changed.get("result").is_some(), "{changed}");
        }
        editor.answer_permission(
            &permission,
            json!({"outcome":{"outcome":"selected","optionId":"allow-once"}}),
        );
        let frames = editor.play(3);
        assert_eq!(
            frames.last().unwrap()["result"]["stopReason"],
            "end_turn",
            "{frames:?}"
        );
        assert_eq!(
            std::fs::read_to_string(project.join("notes.txt")).unwrap(),
            match scenario {
                "changed" => "external",
                "plan" => "before",
                _ => "after",
            }
        );
        assert_eq!(editor.finish(), Some(0));
    }
}

/// Answer every connection with `respond(request)` — decided by what the
/// request carries, never by arrival order, so a preflight or retry
/// connection cannot shift the script.
fn spawn_model(respond: fn(&str) -> String) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    thread::spawn(move || {
        while let Ok((mut stream, _)) = listener.accept() {
            let body = respond(&read_request(&mut stream));
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
            let _ = stream.shutdown(Shutdown::Both);
        }
    });
    addr
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = stream.read(&mut chunk).unwrap_or(0);
        if read == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..read]);
        if let Some(header_end) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&buf[..header_end]).to_ascii_lowercase();
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if buf.len() >= header_end + 4 + length {
                break;
            }
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn temp_dir(name: &str) -> Dir {
    let dir = std::env::temp_dir().join(format!(
        "rapidlm-acpcli-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    Dir(dir)
}

fn rapid(project: &Path, home: &Path, config: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rapid"));
    command
        .current_dir(project)
        .env("HOME", home)
        .env("RAPIDLM_CONFIG", config)
        .env_remove("RAPIDLM_HOME")
        .env_remove("RAPIDLM_MODEL")
        .env_remove("RAPIDLM_PERMISSION_MODE");
    command
}

/// A trusted git project whose model is `respond`, under the default
/// permission mode (a workspace write asks). Returns `(project, config)`.
fn trusted_project(home: &Path, respond: fn(&str) -> String) -> (PathBuf, PathBuf) {
    let project = home.join("project");
    std::fs::create_dir_all(project.join(".rapidlm")).expect("project");
    let init = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&project)
        .output()
        .expect("git init");
    assert!(init.status.success(), "git init failed");
    let config = home.join("config.toml");
    let addr = spawn_model(respond);
    std::fs::write(&config, config_doc(&format!("http://{addr}/v1"))).expect("config");
    let trust = rapid(&project, home, &config)
        .args(["trust", "grant"])
        .output()
        .expect("trust grant");
    assert!(
        trust.status.success(),
        "{}{}",
        String::from_utf8_lossy(&trust.stdout),
        String::from_utf8_lossy(&trust.stderr)
    );
    (project, config)
}

/// The editor: writes frames to `rapid acp`'s stdin, reads its stdout.
struct Editor {
    child: Child,
    stdin: Option<ChildStdin>,
    frames: Receiver<Value>,
    stderr: PathBuf,
}

impl Editor {
    fn spawn(project: &Path, home: &Path, config: &Path) -> Self {
        Self::spawn_from(rapid(project, home, config), home)
    }

    /// `rapid acp` from `command` (a [`rapid`] command, perhaps with more
    /// environment).
    fn spawn_from(mut command: Command, home: &Path) -> Self {
        let stderr = home.join("acp.stderr");
        let mut child = command
            .arg("acp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(&stderr).expect("stderr file"))
            .spawn()
            .expect("spawn rapid acp");
        let stdout = child.stdout.take().expect("stdout");
        let (tx, frames) = std::sync::mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                    return;
                };
                if tx.send(frame).is_err() {
                    return;
                }
            }
        });
        let stdin = child.stdin.take();
        Self {
            child,
            stdin,
            frames,
            stderr,
        }
    }

    fn send(&mut self, frame: Value) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        let mut line = frame.to_string();
        line.push('\n');
        stdin.write_all(line.as_bytes()).expect("write frame");
        stdin.flush().expect("flush frame");
    }

    fn request(&mut self, id: i64, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
    }

    /// The next frame, or `None` once the serve's stdout closed or the
    /// deadline passed.
    fn next(&self, deadline: Instant) -> Option<Value> {
        let left = deadline.checked_duration_since(Instant::now())?;
        self.frames.recv_timeout(left).ok()
    }

    /// Frames until the result of request `id`.
    fn until_result(&self, id: i64) -> Value {
        let deadline = Instant::now() + DEADLINE;
        while let Some(frame) = self.next(deadline) {
            if frame["id"] == id && frame.get("method").is_none() {
                return frame;
            }
        }
        panic!(
            "no response to request {id}; stderr: {}",
            self.stderr_text()
        );
    }

    fn stderr_text(&self) -> String {
        std::fs::read_to_string(&self.stderr).unwrap_or_default()
    }

    /// `initialize` + `session/new`; the new session's id.
    fn open_session(&mut self, project: &Path) -> String {
        self.request(1, "initialize", json!({ "protocolVersion": 1 }));
        let init = self.until_result(1);
        assert!(init.get("result").is_some(), "{init}");
        self.request(
            2,
            "session/new",
            json!({ "cwd": project.display().to_string(), "mcpServers": [] }),
        );
        let created = self.until_result(2);
        created["result"]["sessionId"]
            .as_str()
            .unwrap_or_else(|| panic!("no session id: {created}"))
            .to_owned()
    }

    /// Send `session/prompt` as request `id`.
    fn start_prompt(&mut self, id: i64, session: &str, text: &str) {
        self.request(
            id,
            "session/prompt",
            json!({ "sessionId": session, "prompt": [{ "type": "text", "text": text }] }),
        );
    }

    /// Frames until the next `session/request_permission`, which is
    /// returned (not answered).
    fn until_permission(&self) -> Value {
        let deadline = Instant::now() + DEADLINE;
        while let Some(frame) = self.next(deadline) {
            if frame["method"] == "session/request_permission" {
                return frame;
            }
        }
        panic!("no permission request; stderr: {}", self.stderr_text());
    }

    /// Answer the permission request `request` with `result`.
    fn answer_permission(&mut self, request: &Value, result: Value) {
        self.send(json!({ "jsonrpc": "2.0", "id": request["id"].clone(), "result": result }));
    }

    /// Send `session/prompt` as request `id` and play the editor until its
    /// response (see [`Editor::play`]).
    fn prompt(&mut self, id: i64, session: &str, text: &str) -> Vec<Value> {
        self.start_prompt(id, session, text);
        self.play(id)
    }

    /// Play the editor until the response to request `id`: every
    /// `session/request_permission` is answered "allow once" in the
    /// protocol's response shape. Returns every frame seen, the response
    /// last. Panics if the serve exits first.
    fn play(&mut self, id: i64) -> Vec<Value> {
        let deadline = Instant::now() + DEADLINE;
        let mut seen = Vec::new();
        while let Some(frame) = self.next(deadline) {
            seen.push(frame.clone());
            if frame["method"] == "session/request_permission" {
                self.send(json!({
                    "jsonrpc": "2.0",
                    "id": frame["id"].clone(),
                    "result": { "outcome": { "outcome": "selected", "optionId": "allow-once" } },
                }));
            } else if frame["id"] == id && frame.get("method").is_none() {
                return seen;
            }
        }
        panic!(
            "prompt {id} never answered; frames: {seen:#?}; stderr: {}",
            self.stderr_text()
        );
    }

    /// Disconnect (close stdin) and wait for the exit code.
    fn finish(mut self) -> Option<i32> {
        drop(self.stdin.take());
        let deadline = Instant::now() + DEADLINE;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return status.code(),
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(20));
                }
                _ => {
                    let _ = self.child.kill();
                    panic!("rapid acp did not exit after disconnect");
                }
            }
        }
    }
}

/// Every ledger event of `session` as `(kind, payload)`, through the real
/// export command, read once `rapid acp` has exited.
fn ledger(project: &Path, home: &Path, config: &Path, session: &str) -> Vec<(String, Value)> {
    let out = home.join(format!("export-{session}.jsonl"));
    let export = rapid(project, home, config)
        .args(["inspect-export", session, out.to_str().expect("utf-8 path")])
        .output()
        .expect("inspect-export");
    assert!(
        export.status.success(),
        "{}",
        String::from_utf8_lossy(&export.stderr)
    );
    std::fs::read_to_string(&out)
        .expect("export file")
        .lines()
        .map(|line| {
            let event: Value = serde_json::from_str(line).expect("export line");
            (
                event["kind"].as_str().unwrap_or_default().to_owned(),
                event["payload"].clone(),
            )
        })
        .collect()
}

fn count_kind(events: &[(String, Value)], kind: &str) -> usize {
    events.iter().filter(|(event, _)| event == kind).count()
}

fn permission_requests(frames: &[Value]) -> usize {
    frames
        .iter()
        .filter(|frame| frame["method"] == "session/request_permission")
        .count()
}

#[test]
fn every_permission_request_of_one_prompt_reaches_the_prompt_and_it_completes() {
    // Two writes, each asking: the second is raised by the continuation the
    // first answer resumed. Then the answer.
    fn model(request: &str) -> String {
        match tool_results(request) {
            0 => write_call("call_1", "first.txt"),
            1 => write_call("call_2", "second.txt"),
            _ => answer("both files handled"),
        }
    }
    let home = temp_dir("two-permissions");
    let (project, config) = trusted_project(&home.0, model);
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);

    let frames = editor.prompt(3, &session, "write both files");
    let stderr = editor.stderr_text();
    assert_eq!(
        permission_requests(&frames),
        2,
        "{frames:#?}\nstderr: {stderr}"
    );
    let response = frames.last().expect("the prompt's response");
    assert_eq!(
        response["result"]["stopReason"], "end_turn",
        "{frames:#?}\nstderr: {stderr}"
    );
    // The serve is still up: a clean disconnect exits 0, not the protocol
    // failure a misrouted answer causes.
    assert_eq!(editor.finish(), Some(0), "stderr: {stderr}");
}

/// One write that asks, then an answer — a prompt that pauses once.
fn one_write(request: &str) -> String {
    match tool_results(request) {
        0 => write_call("call_1", "first.txt"),
        _ => answer("resumed"),
    }
}

#[test]
fn a_cancel_while_the_turn_waits_on_an_approval_ends_the_prompt_and_nothing_resumes() {
    let home = temp_dir("cancel-while-waiting");
    let (project, config) = trusted_project(&home.0, one_write);
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);

    editor.start_prompt(3, &session, "write it");
    // Surfaced only once the turn's pause is recorded: the cancel meets a
    // session with no live turn — the case the kernel cannot interrupt.
    let permission = editor.until_permission();
    // The protocol's order: cancel, then answer the outstanding request.
    editor.send(json!({
        "jsonrpc": "2.0",
        "method": "session/cancel",
        "params": { "sessionId": session },
    }));
    editor.answer_permission(
        &permission,
        json!({ "outcome": { "outcome": "cancelled" } }),
    );
    let frames = editor.play(3);
    let stderr = editor.stderr_text();
    assert_eq!(
        frames.last().expect("response")["result"]["stopReason"],
        "cancelled",
        "{frames:#?}\nstderr: {stderr}"
    );
    // The answer to the cancelled prompt's request is dropped, not fatal.
    assert_eq!(editor.finish(), Some(0), "stderr: {stderr}");
    // Nothing was decided and nothing resumed: the approval stays pending.
    let events = ledger(&project, &home.0, &config, &session);
    assert_eq!(count_kind(&events, "approval.resolved"), 0, "{events:#?}");
    assert_eq!(count_kind(&events, "turn.started"), 1, "{events:#?}");
}

#[test]
fn a_disconnect_while_the_turn_waits_on_an_approval_lets_the_serve_exit() {
    let home = temp_dir("disconnect-while-waiting");
    let (project, config) = trusted_project(&home.0, one_write);
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);

    editor.start_prompt(3, &session, "write it");
    let _unanswered = editor.until_permission();
    let stderr = editor.stderr_text();
    assert_eq!(editor.finish(), Some(0), "stderr: {stderr}");
    let events = ledger(&project, &home.0, &config, &session);
    assert_eq!(count_kind(&events, "approval.resolved"), 0, "{events:#?}");
}

#[test]
fn an_error_answer_decides_nothing_and_ends_the_prompt() {
    let home = temp_dir("error-answer");
    let (project, config) = trusted_project(&home.0, one_write);
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);

    editor.start_prompt(3, &session, "write it");
    let permission = editor.until_permission();
    editor.send(json!({
        "jsonrpc": "2.0",
        "id": permission["id"].clone(),
        "error": { "code": -32601, "message": "Method not found" },
    }));
    let frames = editor.play(3);
    let stderr = editor.stderr_text();
    assert_eq!(
        frames.last().expect("response")["result"]["stopReason"],
        "refusal",
        "{frames:#?}\nstderr: {stderr}"
    );
    assert_eq!(editor.finish(), Some(0), "stderr: {stderr}");
    // No decision is recorded that nobody made, and nothing resumed.
    let events = ledger(&project, &home.0, &config, &session);
    assert_eq!(count_kind(&events, "approval.resolved"), 0, "{events:#?}");
    assert_eq!(count_kind(&events, "turn.started"), 1, "{events:#?}");
}

/// What a [`one_write`] prompt left behind once its permission request was
/// answered with `result` and the editor disconnected.
struct Answered {
    frames: Vec<Value>,
    stderr: String,
    exit: Option<i32>,
    /// `first.txt` in the project, if the write ran.
    written: Option<String>,
    events: Vec<(String, Value)>,
}

fn answer_one_write(name: &str, result: Value) -> Answered {
    let home = temp_dir(name);
    let (project, config) = trusted_project(&home.0, one_write);
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);

    editor.start_prompt(3, &session, "write it");
    let permission = editor.until_permission();
    assert_eq!(
        permission["params"]["toolCall"]["toolCallId"], "call_1",
        "{permission}"
    );
    editor.answer_permission(&permission, result);
    let frames = editor.play(3);
    let stderr = editor.stderr_text();
    let exit = editor.finish();
    Answered {
        frames,
        stderr,
        exit,
        written: std::fs::read_to_string(project.join("first.txt")).ok(),
        events: ledger(&project, &home.0, &config, &session),
    }
}

/// The `decision` of every `approval.resolved` in `events`, in order.
fn decisions(events: &[(String, Value)]) -> Vec<String> {
    events
        .iter()
        .filter(|(kind, _)| kind == "approval.resolved")
        .map(|(_, payload)| payload["decision"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[test]
fn an_allow_once_answer_runs_the_write_it_approves() {
    let run = answer_one_write(
        "allow-once",
        json!({ "outcome": { "outcome": "selected", "optionId": "allow-once" } }),
    );
    let (frames, stderr, events) = (&run.frames, &run.stderr, &run.events);
    assert_eq!(
        frames.last().expect("response")["result"]["stopReason"],
        "end_turn",
        "{frames:#?}\nstderr: {stderr}"
    );
    assert_eq!(run.exit, Some(0), "stderr: {stderr}");
    assert_eq!(decisions(events), ["approved"], "{events:#?}");
    assert_eq!(
        run.written.as_deref(),
        Some("from the model"),
        "the approved write never ran: {events:#?}\nstderr: {stderr}"
    );
}

#[test]
fn a_reject_once_answer_leaves_the_project_untouched() {
    let run = answer_one_write(
        "reject-once",
        json!({ "outcome": { "outcome": "selected", "optionId": "reject-once" } }),
    );
    let (frames, stderr, events) = (&run.frames, &run.stderr, &run.events);
    // Denied, the turn resumes and the model answers.
    assert_eq!(
        frames.last().expect("response")["result"]["stopReason"],
        "end_turn",
        "{frames:#?}\nstderr: {stderr}"
    );
    assert_eq!(run.exit, Some(0), "stderr: {stderr}");
    assert_eq!(decisions(events), ["denied"], "{events:#?}");
    assert_eq!(run.written, None, "a rejected write ran");
}

#[test]
fn a_cancelled_answer_is_a_cancel_not_a_denial() {
    // The editor answers `cancelled` without a `session/cancel` ahead of
    // it: the prompt ends as the cancel would end it.
    let run = answer_one_write(
        "cancelled-answer",
        json!({ "outcome": { "outcome": "cancelled" } }),
    );
    let (frames, stderr, events) = (&run.frames, &run.stderr, &run.events);
    assert_eq!(
        frames.last().expect("response")["result"]["stopReason"],
        "cancelled",
        "{frames:#?}\nstderr: {stderr}"
    );
    assert_eq!(run.exit, Some(0), "stderr: {stderr}");
    // Nothing was decided and nothing resumed: the approval stays pending.
    assert_eq!(count_kind(events, "approval.resolved"), 0, "{events:#?}");
    assert_eq!(count_kind(events, "turn.started"), 1, "{events:#?}");
    assert_eq!(run.written, None);
}

#[test]
fn an_answer_outside_the_protocol_s_shape_decides_nothing() {
    // An "allow once" only in appearance: `outcome` not nested, or an
    // array where the protocol has an object.
    for (name, answer) in [
        (
            "flat-answer",
            json!({ "outcome": "selected", "optionId": "allow-once" }),
        ),
        (
            "array-answer",
            json!({ "outcome": ["selected", "allow-once"] }),
        ),
    ] {
        let run = answer_one_write(name, answer);
        let (frames, stderr, events) = (&run.frames, &run.stderr, &run.events);
        assert_eq!(
            frames.last().expect("response")["result"]["stopReason"],
            "refusal",
            "{name}: {frames:#?}\nstderr: {stderr}"
        );
        assert!(
            stderr.contains("not an offered option"),
            "{name}: stderr: {stderr}"
        );
        assert_eq!(run.exit, Some(0), "{name}: stderr: {stderr}");
        assert_eq!(
            count_kind(events, "approval.resolved"),
            0,
            "{name}: {events:#?}"
        );
        assert_eq!(count_kind(events, "turn.started"), 1, "{name}: {events:#?}");
        assert_eq!(run.written, None, "{name}");
    }
}

/// The option ids a `session/request_permission` offers, in order.
fn offered(permission: &Value) -> Vec<String> {
    permission["params"]["options"]
        .as_array()
        .map(|options| {
            options
                .iter()
                .map(|option| option["optionId"].as_str().unwrap_or_default().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// `rapid permissions list` for `project`.
fn permissions_list(project: &Path, home: &Path, config: &Path) -> String {
    let listed = rapid(project, home, config)
        .args(["permissions", "list"])
        .output()
        .expect("permissions list");
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    String::from_utf8_lossy(&listed.stdout).into_owned()
}

/// The `remember` flag of every `approval.resolved` in `events`, in order.
fn remembered(events: &[(String, Value)]) -> Vec<bool> {
    events
        .iter()
        .filter(|(kind, _)| kind == "approval.resolved")
        .map(|(_, payload)| payload["remember"].as_bool().unwrap_or_default())
        .collect()
}

/// One prompt of a fresh serve, in a new session, the editor answering any
/// permission request "allow once": the frames, and the session's ledger.
fn prompt_in_a_new_serve(
    command: Command,
    project: &Path,
    home: &Path,
    config: &Path,
) -> (Vec<Value>, String, Vec<(String, Value)>) {
    let mut editor = Editor::spawn_from(command, home);
    let session = editor.open_session(project);
    let frames = editor.prompt(3, &session, "write it");
    let stderr = editor.stderr_text();
    assert_eq!(editor.finish(), Some(0), "stderr: {stderr}");
    let events = ledger(project, home, config, &session);
    (frames, stderr, events)
}

#[test]
fn an_allow_always_answer_is_kept_so_the_same_call_is_not_asked_again() {
    let home = temp_dir("allow-always");
    let (project, config) = trusted_project(&home.0, one_write);
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);

    editor.start_prompt(3, &session, "write it");
    let permission = editor.until_permission();
    // Both standing answers are offered because both are kept.
    assert_eq!(
        offered(&permission),
        ["allow-once", "allow-always", "reject-once", "reject-always"],
        "{permission}"
    );
    editor.answer_permission(
        &permission,
        json!({ "outcome": { "outcome": "selected", "optionId": "allow-always" } }),
    );
    let frames = editor.play(3);
    let stderr = editor.stderr_text();
    assert_eq!(
        frames.last().expect("response")["result"]["stopReason"],
        "end_turn",
        "{frames:#?}\nstderr: {stderr}"
    );
    assert_eq!(editor.finish(), Some(0), "stderr: {stderr}");
    assert_eq!(
        std::fs::read_to_string(project.join("first.txt"))
            .ok()
            .as_deref(),
        Some("from the model"),
        "stderr: {stderr}"
    );
    let events = ledger(&project, &home.0, &config, &session);
    assert_eq!(decisions(&events), ["approved"], "{events:#?}");
    assert_eq!(remembered(&events), [true], "{events:#?}");
    // Kept where the permission lattice reads it, for this exact call.
    let listed = permissions_list(&project, &home.0, &config);
    assert!(
        listed.contains("allow=workspace_write(first.txt)\n"),
        "{listed}"
    );
    assert!(listed.contains("grants=1\n"), "{listed}");

    // The same call in a new serve — as after a restart — runs unasked.
    std::fs::remove_file(project.join("first.txt")).expect("remove first.txt");
    let (frames, stderr, events) = prompt_in_a_new_serve(
        rapid(&project, &home.0, &config),
        &project,
        &home.0,
        &config,
    );
    assert_eq!(
        permission_requests(&frames),
        0,
        "asked again: {frames:#?}\nstderr: {stderr}"
    );
    assert_eq!(count_kind(&events, "approval.requested"), 0, "{events:#?}");
    assert_eq!(
        frames.last().expect("response")["result"]["stopReason"],
        "end_turn",
        "{frames:#?}\nstderr: {stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(project.join("first.txt"))
            .ok()
            .as_deref(),
        Some("from the model"),
        "the remembered write never ran: {events:#?}\nstderr: {stderr}"
    );

    // Managed policy still outranks the kept grant: a banned write is
    // denied, unasked, and never runs.
    std::fs::remove_file(project.join("first.txt")).expect("remove first.txt");
    let policy = home.0.join("managed.toml");
    std::fs::write(
        &policy,
        "schema = \"rapidlm.managed_config.v1\"\n[policy]\ndenied_tools = [\"workspace_write\"]\n",
    )
    .expect("policy");
    let mut managed = rapid(&project, &home.0, &config);
    managed.env("RAPIDLM_MANAGED_CONFIG", &policy);
    let (frames, stderr, events) = prompt_in_a_new_serve(managed, &project, &home.0, &config);
    assert_eq!(
        permission_requests(&frames),
        0,
        "{frames:#?}\nstderr: {stderr}"
    );
    assert!(
        !project.join("first.txt").exists(),
        "a managed ban lost to a kept grant: {events:#?}\nstderr: {stderr}"
    );
    // Denied by the ban itself, and the turn went on to its answer.
    let denials: Vec<&Value> = events
        .iter()
        .filter(|(kind, _)| kind == "tool.denied")
        .map(|(_, payload)| payload)
        .collect();
    assert_eq!(denials.len(), 1, "{events:#?}");
    assert!(
        denials[0]["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("banned by managed policy")),
        "{events:#?}"
    );
    assert_eq!(
        frames.last().expect("response")["result"]["stopReason"],
        "end_turn",
        "{frames:#?}\nstderr: {stderr}"
    );
}

#[test]
fn a_reject_always_answer_is_kept_so_the_same_call_is_refused_unasked() {
    let home = temp_dir("reject-always");
    let (project, config) = trusted_project(&home.0, one_write);
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);
    editor.start_prompt(3, &session, "write it");
    let permission = editor.until_permission();
    editor.answer_permission(
        &permission,
        json!({ "outcome": { "outcome": "selected", "optionId": "reject-always" } }),
    );
    let frames = editor.play(3);
    let stderr = editor.stderr_text();
    assert_eq!(
        frames.last().expect("response")["result"]["stopReason"],
        "end_turn",
        "{frames:#?}\nstderr: {stderr}"
    );
    assert_eq!(editor.finish(), Some(0), "stderr: {stderr}");
    assert!(!project.join("first.txt").exists(), "stderr: {stderr}");
    let events = ledger(&project, &home.0, &config, &session);
    assert_eq!(decisions(&events), ["denied"], "{events:#?}");
    let listed = permissions_list(&project, &home.0, &config);
    assert!(
        listed.contains("deny=workspace_write(first.txt)\n"),
        "{listed}"
    );
    assert!(
        listed.contains("rule=deny workspace_write(first.txt) origin=persisted\n"),
        "{listed}"
    );

    // The same call in a new serve is refused without asking.
    let (frames, stderr, events) = prompt_in_a_new_serve(
        rapid(&project, &home.0, &config),
        &project,
        &home.0,
        &config,
    );
    assert_eq!(
        permission_requests(&frames),
        0,
        "asked again: {frames:#?}\nstderr: {stderr}"
    );
    assert!(!project.join("first.txt").exists(), "{events:#?}");
    let denials: Vec<&Value> = events
        .iter()
        .filter(|(kind, _)| kind == "tool.denied")
        .map(|(_, payload)| payload)
        .collect();
    assert_eq!(denials.len(), 1, "{events:#?}");
    assert!(
        denials[0]["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("never to allow")),
        "{events:#?}"
    );
}

#[test]
fn allow_always_is_not_offered_where_nothing_would_keep_it_and_is_no_answer_there() {
    // A command: a joined argv names no one command, so no grant can keep
    // "always" for it.
    fn model(request: &str) -> String {
        match tool_results(request) {
            0 => tool_call(
                "call_1",
                "shell_exec",
                json!({ "argv": ["git", "--version"] }),
            ),
            _ => answer("resumed"),
        }
    }
    let home = temp_dir("allow-always-not-offered");
    let (project, config) = trusted_project(&home.0, model);
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);

    editor.start_prompt(3, &session, "run it");
    let permission = editor.until_permission();
    assert_eq!(
        offered(&permission),
        ["allow-once", "reject-once"],
        "{permission}"
    );
    // Chosen anyway, it is not an option this request offered: no decision.
    editor.answer_permission(
        &permission,
        json!({ "outcome": { "outcome": "selected", "optionId": "allow-always" } }),
    );
    let frames = editor.play(3);
    let stderr = editor.stderr_text();
    assert_eq!(
        frames.last().expect("response")["result"]["stopReason"],
        "refusal",
        "{frames:#?}\nstderr: {stderr}"
    );
    assert!(stderr.contains("not an offered option"), "stderr: {stderr}");
    assert_eq!(editor.finish(), Some(0), "stderr: {stderr}");
    let events = ledger(&project, &home.0, &config, &session);
    assert_eq!(count_kind(&events, "approval.resolved"), 0, "{events:#?}");
    let listed = permissions_list(&project, &home.0, &config);
    assert!(listed.contains("grants=0\n"), "{listed}");
}

#[test]
fn a_second_prompt_while_the_first_waits_is_refused() {
    let home = temp_dir("overlapping-prompt");
    let (project, config) = trusted_project(&home.0, one_write);
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);

    editor.start_prompt(3, &session, "write it");
    let permission = editor.until_permission();
    editor.start_prompt(4, &session, "and meanwhile this");
    let refused = editor.until_result(4);
    let stderr = editor.stderr_text();
    assert_eq!(
        refused["error"]["code"], -32600,
        "{refused}\nstderr: {stderr}"
    );
    // The first prompt is untouched: answered, it resumes and completes.
    editor.answer_permission(
        &permission,
        json!({ "outcome": { "outcome": "selected", "optionId": "allow-once" } }),
    );
    let frames = editor.play(3);
    assert_eq!(
        frames.last().expect("response")["result"]["stopReason"],
        "end_turn",
        "{frames:#?}\nstderr: {stderr}"
    );
    // Its end frees the session: the next prompt is accepted at once.
    let next = editor.prompt(5, &session, "now this");
    assert_eq!(
        next.last().expect("response")["result"]["stopReason"],
        "end_turn",
        "{next:#?}"
    );
    assert_eq!(editor.finish(), Some(0), "stderr: {stderr}");
    let events = ledger(&project, &home.0, &config, &session);
    assert!(
        !events.iter().any(
            |(kind, payload)| kind == "turn.started" && payload["text"] == "and meanwhile this"
        ),
        "the refused prompt never became a turn: {events:#?}"
    );
}

#[test]
fn only_the_approval_the_turn_suspended_on_is_asked() {
    // One model step proposes two writes; both ask, and the turn suspends
    // on the first. Only that one can be resumed, so only it is surfaced.
    fn model(request: &str) -> String {
        match tool_results(request) {
            0 => write_calls(&[("call_a", "a.txt"), ("call_b", "b.txt")]),
            _ => answer("done"),
        }
    }
    let home = temp_dir("parallel-asks");
    let (project, config) = trusted_project(&home.0, model);
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);

    editor.start_prompt(3, &session, "write both");
    let permission = editor.until_permission();
    assert_eq!(
        permission["params"]["toolCall"]["toolCallId"], "call_a",
        "{permission}"
    );
    // Unanswered, nothing else is asked: the turn is paused on this one.
    let settle = Instant::now() + Duration::from_secs(1);
    let mut meanwhile = Vec::new();
    while let Some(frame) = editor.next(settle) {
        meanwhile.push(frame);
    }
    assert_eq!(permission_requests(&meanwhile), 0, "{meanwhile:#?}");
    editor.answer_permission(
        &permission,
        json!({ "outcome": { "outcome": "selected", "optionId": "allow-once" } }),
    );
    let frames = editor.play(3);
    let stderr = editor.stderr_text();
    assert_eq!(
        frames.last().expect("response")["result"]["stopReason"],
        "end_turn",
        "{frames:#?}\nstderr: {stderr}"
    );
    assert_eq!(editor.finish(), Some(0), "stderr: {stderr}");
}

/// How many of `frames` carry `needle` anywhere.
fn mentions(frames: &[Value], needle: &str) -> usize {
    frames
        .iter()
        .filter(|frame| frame.to_string().contains(needle))
        .count()
}

#[test]
fn the_next_prompt_streams_only_its_own_turn() {
    // Each prompt proposes one write of its own (one approval each), then
    // finishes. The second must stream its own tool activity and approval,
    // and none of the first's.
    fn model(request: &str) -> String {
        if request.contains("PROMPT-TWO") {
            if request.contains("call_second") {
                answer("second done")
            } else {
                write_call("call_second", "second.txt")
            }
        } else if tool_results(request) == 0 {
            write_call("call_first", "first.txt")
        } else {
            answer("first done")
        }
    }
    let home = temp_dir("two-prompts");
    let (project, config) = trusted_project(&home.0, model);
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);

    let first = editor.prompt(3, &session, "PROMPT-ONE write it");
    let stderr = editor.stderr_text();
    // What the second prompt must not repeat, seen once where it belongs.
    assert_eq!(
        permission_requests(&first),
        1,
        "{first:#?}\nstderr: {stderr}"
    );
    assert!(mentions(&first, "call_first") > 0, "{first:#?}");
    assert_eq!(
        first.last().expect("response")["result"]["stopReason"],
        "end_turn"
    );

    let second = editor.prompt(4, &session, "PROMPT-TWO write another");
    let stderr = editor.stderr_text();
    assert_eq!(
        mentions(&second, "call_first"),
        0,
        "the first turn's tool activity replayed: {second:#?}\nstderr: {stderr}"
    );
    let asked: Vec<&Value> = second
        .iter()
        .filter(|frame| frame["method"] == "session/request_permission")
        .collect();
    assert_eq!(
        asked.len(),
        1,
        "only the second turn's own approval: {second:#?}"
    );
    assert_eq!(asked[0]["params"]["toolCall"]["toolCallId"], "call_second");
    assert!(mentions(&second, "call_second") > 1, "{second:#?}");
    assert_eq!(
        second.last().expect("response")["result"]["stopReason"],
        "end_turn"
    );
    assert_eq!(editor.finish(), Some(0), "stderr: {stderr}");
}

/// The `reason` of every `turn.interrupted` in `events`, in order.
fn interruptions(events: &[(String, Value)]) -> Vec<String> {
    events
        .iter()
        .filter(|(kind, _)| kind == "turn.interrupted")
        .map(|(_, payload)| payload["reason"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[test]
fn a_disconnect_while_the_turn_runs_interrupts_it() {
    // The model takes its time; the editor leaves while it thinks. Left
    // running, the turn would go on to propose a write and ask.
    static THINKING: AtomicBool = AtomicBool::new(false);
    fn model(request: &str) -> String {
        match tool_results(request) {
            0 => {
                THINKING.store(true, Ordering::SeqCst);
                thread::sleep(Duration::from_secs(2));
                write_call("call_1", "first.txt")
            }
            _ => answer("done"),
        }
    }
    let home = temp_dir("disconnect-while-running");
    let (project, config) = trusted_project(&home.0, model);
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);

    editor.start_prompt(3, &session, "write it");
    let deadline = Instant::now() + DEADLINE;
    while !THINKING.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the model was never asked");
        thread::sleep(Duration::from_millis(10));
    }
    let stderr = editor.stderr_text();
    assert_eq!(editor.finish(), Some(0), "stderr: {stderr}");
    let events = ledger(&project, &home.0, &config, &session);
    assert_eq!(
        interruptions(&events),
        ["client_requested"],
        "interrupted at the disconnect: {events:#?}"
    );
    assert_eq!(count_kind(&events, "approval.requested"), 0, "{events:#?}");
}

#[test]
fn a_prompt_straight_after_a_cancel_is_accepted() {
    fn model(request: &str) -> String {
        if request.contains("PROMPT-TWO") {
            answer("second")
        } else {
            one_write(request)
        }
    }
    let home = temp_dir("prompt-after-cancel");
    let (project, config) = trusted_project(&home.0, model);
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);

    editor.start_prompt(3, &session, "write it");
    let permission = editor.until_permission();
    // Cancel and prompt again at once, without waiting for the cancelled
    // prompt's response; then answer its request as the protocol asks.
    editor.send(json!({
        "jsonrpc": "2.0",
        "method": "session/cancel",
        "params": { "sessionId": session },
    }));
    editor.start_prompt(4, &session, "PROMPT-TWO instead");
    editor.answer_permission(
        &permission,
        json!({ "outcome": { "outcome": "cancelled" } }),
    );
    let deadline = Instant::now() + DEADLINE;
    let (mut first, mut second) = (None, None);
    while first.is_none() || second.is_none() {
        let frame = editor
            .next(deadline)
            .unwrap_or_else(|| panic!("a prompt never answered; stderr: {}", editor.stderr_text()));
        if frame.get("method").is_none() && frame["id"] == 3 {
            first = Some(frame);
        } else if frame.get("method").is_none() && frame["id"] == 4 {
            second = Some(frame);
        }
    }
    let stderr = editor.stderr_text();
    let (first, second) = (first.expect("3"), second.expect("4"));
    assert_eq!(
        first["result"]["stopReason"], "cancelled",
        "{first}\nstderr: {stderr}"
    );
    assert_eq!(
        second["result"]["stopReason"], "end_turn",
        "{second}\nstderr: {stderr}"
    );
    assert_eq!(editor.finish(), Some(0), "stderr: {stderr}");
}

#[test]
fn a_call_id_reused_after_a_cancelled_approval_is_asked_again() {
    // The second prompt's model proposes a call under the same id as the
    // first prompt's, whose approval the cancel left pending. Its
    // continuation is the request that already carries that call.
    fn model(request: &str) -> String {
        if request.contains("PROMPT-TWO") {
            if request.contains("second.txt") {
                answer("second done")
            } else {
                write_call("call_1", "second.txt")
            }
        } else {
            one_write(request)
        }
    }
    let home = temp_dir("reused-call-id");
    let (project, config) = trusted_project(&home.0, model);
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);

    editor.start_prompt(3, &session, "write it");
    let permission = editor.until_permission();
    editor.send(json!({
        "jsonrpc": "2.0",
        "method": "session/cancel",
        "params": { "sessionId": session },
    }));
    editor.answer_permission(
        &permission,
        json!({ "outcome": { "outcome": "cancelled" } }),
    );
    let first = editor.play(3);
    assert_eq!(
        first.last().expect("response")["result"]["stopReason"],
        "cancelled"
    );

    let second = editor.prompt(4, &session, "PROMPT-TWO write again");
    let stderr = editor.stderr_text();
    assert_eq!(
        permission_requests(&second),
        1,
        "the reused call's own approval is asked: {second:#?}\nstderr: {stderr}"
    );
    assert_eq!(
        second.last().expect("response")["result"]["stopReason"],
        "end_turn",
        "{second:#?}\nstderr: {stderr}"
    );
    assert_eq!(editor.finish(), Some(0), "stderr: {stderr}");
}

#[cfg(unix)]
#[test]
fn a_disconnect_while_a_command_runs_stops_it_before_the_serve_exits() {
    fn model(request: &str) -> String {
        match tool_results(request) {
            0 => tool_call(
                "call_1",
                "shell_exec",
                json!({ "argv": ["sleep", sleep_marker(1)] }),
            ),
            _ => answer("done"),
        }
    }
    let marker = sleep_marker(1);
    let running = || sleeping(&marker);
    let home = temp_dir("disconnect-while-command-runs");
    let (project, config) = trusted_project(&home.0, model);
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);
    // The command runs without asking.
    editor.request(
        3,
        "session/set_mode",
        json!({ "sessionId": session, "mode": "bypassPermissions" }),
    );
    let set = editor.until_result(3);
    assert!(set.get("result").is_some(), "{set}");
    editor.start_prompt(4, &session, "run it");
    let deadline = Instant::now() + DEADLINE;
    while !running() {
        assert!(
            Instant::now() < deadline,
            "the command never started; stderr: {}",
            editor.stderr_text()
        );
        thread::sleep(Duration::from_millis(20));
    }
    let stderr = editor.stderr_text();
    let code = editor.finish();
    let orphaned = running();
    if orphaned {
        let _ = Command::new("pkill")
            .args(["-f", &format!("sleep {marker}")])
            .status();
    }
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(!orphaned, "the interrupted command outlived the serve");
}

#[cfg(unix)]
#[test]
fn a_cancel_while_a_command_runs_keeps_its_end_out_of_the_next_prompt() {
    // The cancelled turn's command is stopped and its end recorded after
    // the kernel records the interruption — here about two seconds after,
    // while a background child still holds the command's output open. The
    // next prompt, sent at once and still running then (its model takes
    // four), must not stream that end as its own.
    fn model(request: &str) -> String {
        if request.contains("PROMPT-TWO") {
            thread::sleep(Duration::from_secs(4));
            answer("second")
        } else {
            match tool_results(request) {
                0 => tool_call(
                    "call_old",
                    "shell_exec",
                    json!({ "argv": [
                        "sh",
                        "-c",
                        format!("sleep 2 & exec sleep {}", sleep_marker(2)),
                    ] }),
                ),
                _ => answer("done"),
            }
        }
    }
    let marker = sleep_marker(2);
    let home = temp_dir("cancel-while-command-runs");
    let (project, config) = trusted_project(&home.0, model);
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);
    editor.request(
        3,
        "session/set_mode",
        json!({ "sessionId": session, "mode": "bypassPermissions" }),
    );
    let set = editor.until_result(3);
    assert!(set.get("result").is_some(), "{set}");
    editor.start_prompt(4, &session, "run it");
    let deadline = Instant::now() + DEADLINE;
    while !sleeping(&marker) {
        assert!(
            Instant::now() < deadline,
            "the command never started; stderr: {}",
            editor.stderr_text()
        );
        thread::sleep(Duration::from_millis(20));
    }
    editor.send(json!({
        "jsonrpc": "2.0",
        "method": "session/cancel",
        "params": { "sessionId": session },
    }));
    editor.start_prompt(5, &session, "PROMPT-TWO now");
    let (mut first, mut second) = (None, None);
    let mut after_first = Vec::new();
    while second.is_none() {
        let frame = editor
            .next(deadline)
            .unwrap_or_else(|| panic!("a prompt never answered; stderr: {}", editor.stderr_text()));
        if frame.get("method").is_none() && frame["id"] == 4 {
            first = Some(frame);
        } else if frame.get("method").is_none() && frame["id"] == 5 {
            second = Some(frame);
        } else if first.is_some() {
            after_first.push(frame);
        }
    }
    let stderr = editor.stderr_text();
    let first = first.expect("the cancelled prompt answers before the next one ends");
    assert_eq!(
        first["result"]["stopReason"], "cancelled",
        "{first}\nstderr: {stderr}"
    );
    assert_eq!(
        second.expect("5")["result"]["stopReason"],
        "end_turn",
        "stderr: {stderr}"
    );
    assert_eq!(
        mentions(&after_first, "call_old"),
        0,
        "the cancelled turn's command end streamed into the next prompt: {after_first:#?}"
    );
    let stopped = !sleeping(&marker);
    if !stopped {
        let _ = Command::new("pkill")
            .args(["-f", &format!("sleep {marker}")])
            .status();
    }
    assert!(stopped, "the cancelled command was stopped");
    assert_eq!(editor.finish(), Some(0), "stderr: {stderr}");
}
