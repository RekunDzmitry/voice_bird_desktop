use tempfile::TempDir;
use voice_bird::session::recover;
use voice_bird::session::writer::{SegmentWriter, WrittenSegment};

#[test]
fn partial_jsonl_plus_recover_produces_valid_json_and_txt() {
    let dir = TempDir::new().unwrap();
    let jsonl = dir.path().join("transcript.jsonl");
    {
        let mut w = SegmentWriter::open(&jsonl).unwrap();
        w.append(&WrittenSegment { t_start_ms: 0,    t_end_ms: 500,  text: "partial".into() }).unwrap();
        w.append(&WrittenSegment { t_start_ms: 500, t_end_ms: 1000, text: "transcript".into() }).unwrap();
        // Writer dropped without finalize — simulates crash mid-session.
    }

    recover::recover(dir.path()).unwrap();

    let j: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("transcript.json")).unwrap()
    ).unwrap();
    assert_eq!(j["segments"].as_array().unwrap().len(), 2);
    let t = std::fs::read_to_string(dir.path().join("transcript.txt")).unwrap();
    assert_eq!(t, "partial\ntranscript\n");
}
