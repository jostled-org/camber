use crate::context;
use crate::template;
use std::fs;
use std::io::{self, Write};
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
        "http" => scaffold(
            project,
            name,
            template::HTTP_CARGO_TOML,
            template::HTTP_MAIN_RS,
            &[],
        ),
        "fanout" => scaffold(
            project,
            name,
            template::FANOUT_CARGO_TOML,
            template::FANOUT_MAIN_RS,
            &[],
        ),
        "advanced" => scaffold(
            project,
            name,
            template::ADVANCED_CARGO_TOML,
            template::ADVANCED_MAIN_RS,
            &[
                ("build.rs", template::ADVANCED_BUILD_RS),
                ("proto/echo.proto", template::ADVANCED_PROTO),
            ],
        ),
        _ => Err(invalid_input(format!(
            "unknown template '{tmpl}'. available templates: {}",
            template::AVAILABLE_TEMPLATES.join(", ")
        ))),
    }
}

fn validate_project_name(name: &str) -> io::Result<()> {
    if name.contains('/') || name.contains('\\') {
        return Err(invalid_input(format!(
            "project name '{name}' must not contain path separators"
        )));
    }
    match is_cargo_package_name(name) {
        true => Ok(()),
        false => Err(invalid_input(format!(
            "project name '{name}' is not a valid Cargo package name"
        ))),
    }
}

/// A leading ASCII letter or `_`, then ASCII letters, digits, `-`, or `_`.
fn is_cargo_package_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| matches!(first, 'a'..='z' | 'A'..='Z' | '_'))
        && chars.all(|character| matches!(character, 'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_'))
}

fn invalid_input(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

/// Write the files every template ships, then `extra_files`: paths relative
/// to `project`, with their contents.
fn scaffold(
    project: &Path,
    name: &str,
    cargo_template: &str,
    main_template: &str,
    extra_files: &[(&str, &str)],
) -> io::Result<()> {
    let src_dir = project.join("src");
    fs::create_dir_all(&src_dir)?;

    let cargo_toml = cargo_template.replace("{{name}}", name);
    fs::write(project.join("Cargo.toml"), cargo_toml)?;
    fs::write(src_dir.join("main.rs"), main_template)?;
    extra_files
        .iter()
        .try_for_each(|(relative, contents)| write_project_file(project, relative, contents))?;
    write_llms_txt(project)?;

    print_success(name)
}

fn write_project_file(project: &Path, relative: &str, contents: &str) -> io::Result<()> {
    let path = project.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents)
}

fn write_llms_txt(project: &Path) -> io::Result<()> {
    fs::write(project.join("llms.txt"), context::LLMS_TXT)
}

fn print_success(name: &str) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "Created project '{name}'")?;
    writeln!(stdout, "  cd {name} && cargo run")
}
