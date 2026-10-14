fn main() {
  build_ompoverlay();
  tauri_build::build()
}

// ompoverlay.dll, which the launcher injects into games under Wine
// (wine_overlay.rs), embedded in the exe. Built only by the MinGW (Wine) build;
// other targets embed nothing and inject nothing.
fn build_ompoverlay() {
  let source = "ompoverlay/ompoverlay.c";
  let out = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("ompoverlay.dll");
  println!("cargo:rerun-if-changed={}", source);
  println!("cargo:rerun-if-env-changed=CC_i686_pc_windows_gnu");

  if !std::env::var("TARGET").unwrap().ends_with("-windows-gnu") {
    std::fs::write(&out, []).unwrap();
    return;
  }
  // the game is 32-bit whatever the launcher is
  let cc = std::env::var("CC_i686_pc_windows_gnu").unwrap_or_else(|_| "i686-w64-mingw32-gcc".into());
  let status = std::process::Command::new(&cc)
    .args(["-O2", "-Wall", "-Wextra", "-shared", "-static-libgcc", "-s", "-o"])
    .arg(&out)
    .arg(source)
    .args(["-lgdi32", "-luser32"])
    .status()
    .unwrap_or_else(|e| panic!("cannot run {} for {}: {}", cc, source, e));
  assert!(status.success(), "{} failed on {}", cc, source);
}
