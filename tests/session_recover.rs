use tempfile::TempDir;

use voice_bird_cli::session::recover;
use voice_bird_cli::session::writer::{SegmentWriter, WrittenSegment};

#[test]
fn recover_regenerates_json_and_txt_from_partial_jsonl() {
    let dir = TempDir::new().unwrap();
    let jsonl = dir.path().join("transcript.jsonl");
    {
        let mut w = SegmentWriter::open(&jsonl).unwrap();
        w.append(&WrittenSegment {
            t_start_ms: 0,
            t_end_ms: 1000,
            text: "recovered".into(),
        })
        .unwrap();
    }
    // No audio.wav needed for recover — duration comes from last segment.

    recover::recover(dir.path()).unwrap();

    let json = std::fs::read_to_string(dir.path().join("transcript.json")).unwrap();
    let txt = std::fs::read_to_string(dir.path().join("transcript.txt")).unwrap();
    assert!(json.contains("recovered"));
    assert_eq!(txt, "recovered\n");
}
