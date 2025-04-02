use std::process::Command;
use std::env;

fn main() {
    // Only run this build script in debug mode
    // For release, we assume CSS is pre-built or handled differently
    if env::var("PROFILE").unwrap_or_default() == "debug" {
        println!("cargo:rerun-if-changed=templates/");
        println!("cargo:rerun-if-changed=src/"); // Rerun if Rust code changes (might add/remove classes)
        println!("cargo:rerun-if-changed=input.css");

        // Ensure the output directory exists
        std::fs::create_dir_all("static").expect("Failed to create static directory");

        let output = Command::new("npx")
            .args([
                "@tailwindcss/cli",
                "-i",
                "./input.css",
                "-o",
                "./static/output.css",
                // Add other options like --minify for release if needed later
            ])
            .output() // Capture output for better debugging
            .expect("Failed to execute Tailwind CSS build command");

        if !output.status.success() {
            eprintln!("Tailwind CLI stdout:\n{}", String::from_utf8_lossy(&output.stdout));
            eprintln!("Tailwind CLI stderr:\n{}", String::from_utf8_lossy(&output.stderr));
            panic!("Tailwind CSS build command failed with status: {}", output.status);
        }
        println!("cargo:warning=Tailwind CSS built successfully (debug)");
    } else {
         println!("cargo:warning=Skipping Tailwind CSS build for release profile.");
    }
}