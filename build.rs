/// Build script for Voice Bird Desktop
/// Compiles Protocol Buffer definitions for gRPC communication

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Compile the audio streaming proto file
    tonic_build::configure()
        .build_server(false) // We only need the client
        .build_client(true)
        .compile(
            &["proto/audio_streaming.proto"],
            &["proto/"],
        )?;

    println!("cargo:rerun-if-changed=proto/audio_streaming.proto");

    Ok(())
}
