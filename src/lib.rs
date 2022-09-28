pub mod assert {
    use std::{fs, path::PathBuf, process::Command};

    /// `Assert` is a wrapper around the [`assert_cmd::assert::Assert`]
    /// struct.
    pub struct Assert {
        command: assert_cmd::Command,
        files_to_remove: Option<Vec<PathBuf>>,
    }

    impl Assert {
        pub(crate) fn new(command: Command, files_to_remove: Option<Vec<PathBuf>>) -> Self {
            Self {
                command: assert_cmd::Command::from_std(command),
                files_to_remove,
            }
        }

        pub fn assert(&mut self) -> assert_cmd::assert::Assert {
            self.command.assert()
        }

        /// Shortcut to `self.assert().success()`.
        pub fn success(&mut self) -> assert_cmd::assert::Assert {
            self.assert().success()
        }

        /// Shortcut to `self.assert().failure()`.
        pub fn failure(&mut self) -> assert_cmd::assert::Assert {
            self.assert().failure()
        }
    }

    impl Drop for Assert {
        fn drop(&mut self) {
            if let Some(files_to_remove) = &self.files_to_remove {
                for file in files_to_remove.iter() {
                    if fs::metadata(file).is_ok() {
                        fs::remove_file(file)
                            .unwrap_or_else(|_| panic!("Failed to remove `{:?}`", file));
                    }
                }
            }
        }
    }
}

pub mod run {

    use crate::Assert;
    use lazy_static::lazy_static;
    use regex::Regex;
    use std::{
        borrow::Cow, collections::HashMap, env, error::Error, ffi::OsString, io::prelude::*,
        path::PathBuf, process::Command,
    };

    static INCLUDE_REGEX: &str = "#include \"(.*)\"";

    #[doc(hidden)]
    pub enum Language {
        C,
        Cxx,
    }

    impl ToString for Language {
        fn to_string(&self) -> String {
            match self {
                Self::C => String::from("c"),
                Self::Cxx => String::from("cpp"),
            }
        }
    }

    #[doc(hidden)]
    pub fn run(language: Language, program: &str) -> Result<Assert, Box<dyn Error>> {
        let (program, variables) = collect_environment_variables(program);

        let mut program_file = tempfile::Builder::new()
            .prefix("inline-c-rs-")
            .suffix(&format!(".{}", language.to_string()))
            .tempfile()?;

        program_file.write_all(program.as_bytes())?;

        let host = target_lexicon::HOST.to_string();
        let target = &host;

        let msvc = target.contains("msvc");
        if !msvc {
            panic!("This crate only works with MSVC and Wasmer on Windows!");
        }

        let (_, input_path) = program_file.keep()?;
        let mut output_temp = tempfile::Builder::new();
        let output_temp = output_temp.prefix("inline-c-rs-");
        output_temp.suffix(".exe");

        let (_, output_path) = output_temp.tempfile()?.keep()?;

        let mut build = cc::Build::new();
        let mut build = build
            .cargo_metadata(false)
            .warnings(true)
            .extra_warnings(true)
            .warnings_into_errors(true)
            .debug(false)
            .host(&host)
            .target(target)
            .opt_level(1);

        if let Language::Cxx = language {
            build = build.cpp(true);
        }

        let compiler = build.try_get_compiler()?;
        let mut command = compiler.to_command();

        let cflags = get_env_flags(&variables, "CFLAGS");

        // MSVC cannot follow symlinks for some reason
        let mut log = String::new();
        let include_paths = cflags
            .iter()
            .filter(|s| s.starts_with("-I"))
            .cloned()
            .collect::<Vec<_>>();
        fixup_symlinks(include_paths.as_ref(), &mut log)?;

        let regex = regex::Regex::new(INCLUDE_REGEX).unwrap();
        let filepaths = regex
            .captures_iter(&program)
            .map(|c| c[1].to_string())
            .collect::<Vec<_>>();
        log.push_str(&format!("regex captures (program): {:#?}\n", filepaths));
        let joined_filepaths = filepaths
            .iter()
            .filter_map(|s| {
                let path =
                    std::path::Path::new(&include_paths.first().unwrap().replacen("-I", "", 1))
                        .join(s);
                Some(format!("{}", path.display()))
            })
            .collect::<Vec<_>>();
        fixup_symlinks_inner(&joined_filepaths, &mut log)?;

        let cppflags = get_env_flags(&variables, "CPPFLAGS");
        let cxxflags = get_env_flags(&variables, "CXXFLAGS");
        let ldflags = get_env_flags(&variables, "LDFLAGS");

        command.args(cflags);

        let link_path = ldflags
            .get(0)
            .expect("no link path for .dll")
            .replace("-rpath,", "");
        let mut dll_path = ldflags.get(1).expect("no .dll").clone();
        if dll_path.ends_with(".dll") {
            dll_path = format!("{}.lib", dll_path);
        }
        command_add_output_file(&mut command, &output_path, msvc, compiler.is_like_clang());
        command.arg(input_path.clone());
        command.arg("/link");
        command.arg(dll_path);
        command.arg(format!("/LIBPATH:{}", link_path));

        command.envs(variables.clone());

        let mut files_to_remove = vec![input_path, output_path.clone()];

        let mut intermediate_path = output_path.clone();
        intermediate_path.set_extension("obj");

        files_to_remove.push(intermediate_path);

        Ok(Assert::new(command, Some(files_to_remove)))
    }

    fn collect_environment_variables<'p>(
        program: &'p str,
    ) -> (Cow<'p, str>, HashMap<String, String>) {
        const ENV_VAR_PREFIX: &str = "INLINE_C_RS_";

        lazy_static! {
            static ref REGEX: Regex = Regex::new(
                r#"#inline_c_rs (?P<variable_name>[^:]+):\s*"(?P<variable_value>[^"]+)"\r?\n"#
            )
            .unwrap();
        }

        let mut variables = HashMap::new();

        for (variable_name, variable_value) in env::vars().filter_map(|(mut name, value)| {
            if name.starts_with(ENV_VAR_PREFIX) {
                Some((name.split_off(ENV_VAR_PREFIX.len()), value))
            } else {
                None
            }
        }) {
            variables.insert(variable_name, variable_value);
        }

        for captures in REGEX.captures_iter(program) {
            variables.insert(
                captures["variable_name"].trim().to_string(),
                captures["variable_value"].to_string(),
            );
        }

        let program = REGEX.replace_all(program, "");

        (program, variables)
    }

    // This is copy-pasted and edited from `cc-rs`.
    fn command_add_output_file(
        command: &mut Command,
        output_path: &PathBuf,
        msvc: bool,
        clang: bool,
    ) {
        if msvc && !clang {
            let mut intermediate_path = output_path.clone();
            intermediate_path.set_extension("obj");

            let mut fo_arg = OsString::from("-Fo");
            fo_arg.push(intermediate_path);
            command.arg(fo_arg);

            let mut fe_arg = OsString::from("-Fe");
            fe_arg.push(output_path);
            command.arg(fe_arg);
        } else {
            command.arg("-o").arg(output_path);
        }
    }

    fn get_env_flags(variables: &HashMap<String, String>, env_name: &str) -> Vec<String> {
        variables
            .get(env_name)
            .map(|e| e.to_string())
            .ok_or_else(|| env::var(env_name))
            .unwrap_or_default()
            .split_ascii_whitespace()
            .map(|slice| slice.to_string())
            .collect()
    }

    fn fixup_symlinks(include_paths: &[String], log: &mut String) -> Result<(), Box<dyn Error>> {
        log.push_str(&format!("include paths: {include_paths:?}"));
        for i in include_paths {
            let i = i.replacen("-I", "", 1);
            let mut paths_headers = Vec::new();
            for entry in std::fs::read_dir(&i)? {
                let entry = entry?;
                let path = entry.path();
                let path_display = format!("{}", path.display());
                if path_display.ends_with("h") {
                    paths_headers.push(path_display);
                }
            }
            fixup_symlinks_inner(&paths_headers, log)?;
        }

        Ok(())
    }

    fn fixup_symlinks_inner(
        include_paths: &[String],
        log: &mut String,
    ) -> Result<(), Box<dyn Error>> {
        log.push_str(&format!("fixup symlinks: {include_paths:#?}"));
        let regex = regex::Regex::new(INCLUDE_REGEX).unwrap();
        for path in include_paths.iter() {
            let file = std::fs::read_to_string(&path)?;
            let lines_3 = file.lines().take(3).collect::<Vec<_>>();
            log.push_str(&format!("first 3 lines of {path:?}: {:#?}\n", lines_3));

            let parent = std::path::Path::new(&path).parent().unwrap();
            if let Ok(symlink) = std::fs::read_to_string(parent.clone().join(&file)) {
                log.push_str(&format!("symlinking {path:?}\n"));
                std::fs::write(&path, symlink)?;
            }

            // follow #include directives and recurse
            let filepaths = regex
                .captures_iter(&file)
                .map(|c| c[1].to_string())
                .collect::<Vec<_>>();
            log.push_str(&format!("regex captures: ({path:?}): {:#?}\n", filepaths));
            let joined_filepaths = filepaths
                .iter()
                .filter_map(|s| {
                    let path = parent.clone().join(s);
                    Some(format!("{}", path.display()))
                })
                .collect::<Vec<_>>();
            fixup_symlinks_inner(&joined_filepaths, log)?;
        }
        Ok(())
    }
}

pub use crate::run::{run, Language};
pub use assert::Assert;
pub use wasmer_inline_c_macro::{assert_c, assert_cxx};
pub mod predicates {
    pub use predicates::prelude::*;
}
