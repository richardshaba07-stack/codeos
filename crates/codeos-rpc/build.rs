//! Genera il codice tonic/prost dal contratto `proto/codeos.proto` a build-time.
//!
//! Il codice generato finisce in `OUT_DIR` e viene incluso da `lib.rs` con
//! `tonic::include_proto!("codeos.v1")`.
//!
//! Usiamo un `protoc` **vendored** (binario prebuilt fornito da
//! `protoc-bin-vendored`) così la build non richiede di installare il compilatore
//! protobuf sul sistema: punta `tonic-build` ad esso via la variabile `PROTOC`.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    std::env::set_var("PROTOC", protoc);

    tonic_build::configure().compile(&["proto/codeos.proto"], &["proto"])?;

    // Ricompila solo se cambia il contratto.
    println!("cargo:rerun-if-changed=proto/codeos.proto");
    Ok(())
}
