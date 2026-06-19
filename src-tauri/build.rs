fn main() {
    tauri_build::build();

    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        // Use our custom .rc file that has ONLY the manifest (no VERSION resource)
        res.set_resource_file("res/app.rc");
        res.compile().expect("Failed to compile resources (admin manifest)");
    }
}
