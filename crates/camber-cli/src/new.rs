use crate::context;
use crate::template;
use std::fs;
use std::io;
use std::path::Path;

pub fn run(name: &str, tmpl: &str) -> io::Result<()> {
    validate_project_name(name)?;
    let project = Path::new(name);

    if project.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("directory '{name}' already exists"),
        ));
    }

    match tmpl {
        "http" => scaffold_http(project, name),
        "fanout" => scaffold_fanout(project, name),
        "advanced" => scaffold_advanced(project, name),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "unknown template '{tmpl}'. available templates: {}",
                template::AVAILABLE_TEMPLATES.join(", ")
            ),
        )),
    }
}

fn validate_project_name(name: &str) -> io::Result<()> {
    if name.contains('/') || name.contains('\\') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("project name '{name}' must not contain path separators"),
        ));
    }

    match name {
        "." | ".." | "" => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("project name '{name}' is not a valid Cargo package name"),
            ));
        }
        _ => {}
    }

    let mut chars = name.chars();
    let Some(first_char) = chars.next() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("project name '{name}' is not a valid Cargo package name"),
        ));
    };

    if !matches!(first_char, 'a'..='z' | 'A'..='Z' | '_') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("project name '{name}' is not a valid Cargo package name"),
        ));
    }

    if chars.any(|character| !matches!(character, 'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_')) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("project name '{name}' is not a valid Cargo package name"),
        ));
    }

    Ok(())
}

fn scaffold(
    project: &Path,
    name: &str,
    cargo_template: &str,
    main_template: &str,
) -> io::Result<()> {
    let src_dir = project.join("src");
    fs::create_dir_all(&src_dir)?;

    let cargo_toml = cargo_template.replace("{{name}}", name);
    fs::write(project.join("Cargo.toml"), cargo_toml)?;
    fs::write(src_dir.join("main.rs"), main_template)?;
    write_llms_txt(project)?;

    print_success(name);
    Ok(())
}

fn scaffold_http(project: &Path, name: &str) -> io::Result<()> {
    scaffold(
        project,
        name,
        template::HTTP_CARGO_TOML,
        template::HTTP_MAIN_RS,
    )
}

fn scaffold_fanout(project: &Path, name: &str) -> io::Result<()> {
    scaffold(
        project,
        name,
        template::FANOUT_CARGO_TOML,
        template::FANOUT_MAIN_RS,
    )
}

fn scaffold_advanced(project: &Path, name: &str) -> io::Result<()> {
    let src_dir = project.join("src");
    let proto_dir = project.join("proto");
    fs::create_dir_all(&src_dir)?;
    fs::create_dir_all(&proto_dir)?;

    let cargo_toml = template::ADVANCED_CARGO_TOML.replace("{{name}}", name);
    fs::write(project.join("Cargo.toml"), cargo_toml)?;
    fs::write(src_dir.join("main.rs"), template::ADVANCED_MAIN_RS)?;
    fs::write(project.join("build.rs"), template::ADVANCED_BUILD_RS)?;
    fs::write(proto_dir.join("echo.proto"), template::ADVANCED_PROTO)?;
    write_llms_txt(project)?;

    print_success(name);
    Ok(())
}

fn write_llms_txt(project: &Path) -> io::Result<()> {
    fs::write(project.join("llms.txt"), context::LLMS_TXT)
}

fn print_success(name: &str) {
    println!("Created project '{name}'");
    println!("  cd {name} && cargo run");
}
