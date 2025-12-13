use std::{
    ffi::OsStr,
    path::Path,
    process::{
        Child, ChildStderr, ChildStdin, ChildStdout, Command, CommandArgs, CommandEnvs, ExitStatus,
        Output, Stdio,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OnDrop {
    Forget,
    Wait,
    Kill,
}

#[derive(Debug)]
struct CommandPipelineConfig {
    pipefail: bool,
    on_drop: OnDrop,
    stdin: Option<Stdio>,
    stdout: Option<Stdio>,
}

#[derive(Debug)]
struct PipelineConfig {
    pipefail: bool,
    on_drop: OnDrop,
}

#[derive(Debug)]
pub struct CommandPipeline {
    piped_commands: Vec<Command>,
    tail_command: Command,
    config: CommandPipelineConfig,
}

#[derive(Debug)]
pub struct Pipeline {
    piped_processes: Vec<Child>,
    // Not actually optional. This is needed because it implements drop.
    tail_process: Option<Child>,
    config: PipelineConfig,

    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
    pub stderr: Option<ChildStderr>,
}

impl CommandPipelineConfig {
    pub fn new() -> Self {
        Self {
            pipefail: false,
            on_drop: OnDrop::Wait,
            stdin: None,
            stdout: None,
        }
    }
}

impl Default for CommandPipelineConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandPipeline {
    pub fn new<S: AsRef<OsStr>>(program: S) -> Self {
        Self {
            piped_commands: Vec::new(),
            tail_command: Command::new(program),
            config: CommandPipelineConfig::new(),
        }
    }

    pub fn arg<S: AsRef<OsStr>>(&mut self, arg: S) -> &mut Self {
        self.tail_command.arg(arg.as_ref());
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.tail_command.args(args);
        self
    }

    pub fn env<K, V>(&mut self, key: K, val: V) -> &mut Self
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.tail_command.env(key, val);
        self
    }

    pub fn envs<I, K, V>(&mut self, vars: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.tail_command.envs(vars);
        self
    }

    pub fn env_remove<K: AsRef<OsStr>>(&mut self, key: K) -> &mut Self {
        self.tail_command.env_remove(key);
        self
    }

    pub fn env_clear(&mut self) -> &mut Self {
        self.tail_command.env_clear();
        self
    }

    pub fn current_dir<P: AsRef<Path>>(&mut self, dir: P) -> &mut Self {
        self.tail_command.current_dir(dir);
        self
    }

    pub fn stdin<T: Into<Stdio>>(&mut self, cfg: T) -> &mut Self {
        self.config.stdin = Some(cfg.into());
        self
    }

    pub fn stdout<T: Into<Stdio>>(&mut self, cfg: T) -> &mut Self {
        self.config.stdout = Some(cfg.into());
        self
    }

    pub fn stderr<T: Into<Stdio>>(&mut self, cfg: T) -> &mut Self {
        self.tail_command.stderr(cfg);
        self
    }

    pub fn on_drop(&mut self, cfg: OnDrop) -> &mut Self {
        self.config.on_drop = cfg;
        self
    }

    pub fn pipe<S: AsRef<OsStr>>(&mut self, program: S) -> &mut Self {
        let command = std::mem::replace(&mut self.tail_command, Command::new(program));
        self.piped_commands.push(command);
        self
    }

    pub fn spawn(&mut self) -> std::io::Result<Pipeline> {
        fn post_error_wait_all(processes: Vec<Child>) {
            for mut process in processes {
                let _ = process.wait();
            }
        }

        if let Some(stdin) = self.config.stdin.take() {
            self.piped_commands
                .first_mut()
                .unwrap_or(&mut self.tail_command)
                .stdin(stdin);
        }

        let mut last_stdout = None;
        let mut piped_processes = Vec::with_capacity(self.piped_commands.len());
        let mut try_spawn_commands = || {
            for command in &mut *self.piped_commands {
                // Note: this will NEVER match on the first iteration. So it won't override the
                // stdin we set from the configuration.
                if let Some(last_stdout) = last_stdout.take() {
                    command.stdin(last_stdout);
                }
                command.stdout(Stdio::piped());
                let mut process = command.spawn()?;
                last_stdout = process.stdout.take();
                piped_processes.push(process);
            }
            Ok(())
        };
        if let Err(error) = try_spawn_commands() {
            drop(last_stdout);
            post_error_wait_all(piped_processes);
            return Err(error);
        }

        if let Some(last_stdout) = last_stdout.take() {
            self.tail_command.stdin(last_stdout);
        }
        if let Some(stdout) = self.config.stdout.take() {
            self.tail_command.stdout(stdout);
        }
        match self.tail_command.spawn() {
            Ok(mut tail_process) => {
                let stdin = piped_processes
                    .first_mut()
                    .unwrap_or(&mut tail_process)
                    .stdin
                    .take();
                let stdout = tail_process.stdout.take();
                let stderr = tail_process.stderr.take();
                Ok(Pipeline {
                    piped_processes,
                    tail_process: Some(tail_process),
                    config: PipelineConfig {
                        pipefail: self.config.pipefail,
                        on_drop: self.config.on_drop,
                    },
                    stdin,
                    stdout,
                    stderr,
                })
            }
            Err(error) => {
                post_error_wait_all(piped_processes);
                Err(error)
            }
        }
    }

    pub fn output(&mut self) -> std::io::Result<Output> {
        self.spawn()?.join_with_output()
    }

    pub fn status(&mut self) -> std::io::Result<ExitStatus> {
        self.spawn()?.join()
    }

    pub fn get_program(&self) -> &OsStr {
        self.tail_command.get_program()
    }

    pub fn get_args(&self) -> CommandArgs<'_> {
        self.tail_command.get_args()
    }

    pub fn get_envs(&self) -> CommandEnvs<'_> {
        self.tail_command.get_envs()
    }

    pub fn get_current_dir(&self) -> Option<&Path> {
        self.tail_command.get_current_dir()
    }
}

impl Pipeline {
    pub fn join(mut self) -> std::io::Result<ExitStatus> {
        // Wait for each process in the pipeline, collecting the first exit status that is not a
        // success if the pipefail flag is enabled.
        let mut first_err_status = None;
        for process in &mut self.piped_processes {
            let result = process.wait();
            if self.config.pipefail
                && first_err_status.is_none()
                && !result
                    .as_ref()
                    .is_ok_and(|exit_status| exit_status.success())
            {
                first_err_status = Some(result);
            }
        }
        let tail_status = self.tail_process.take().unwrap().wait();
        // We've already waited on all the exit codes. No reason to do it again on Drop.
        self.config.on_drop = OnDrop::Forget;

        if self.config.pipefail
            && let Some(err_status) = first_err_status
        {
            // We delayed returning IO errors until we finished waiting for all the processes. Now,
            // we need to return the first IO error encountered.
            let err_status = err_status?;
            let _ = tail_status?;
            Ok(err_status)
        } else {
            tail_status
        }
    }

    pub fn join_with_output(mut self) -> std::io::Result<Output> {
        // Wait for each process in the pipeline, collecting the first exit status that is not a
        // success if the pipefail flag is enabled.
        let mut first_err_status = None;
        for process in &mut self.piped_processes {
            let result = process.wait();
            if self.config.pipefail
                && first_err_status.is_none()
                && !result
                    .as_ref()
                    .is_ok_and(|exit_status| exit_status.success())
            {
                first_err_status = Some(result);
            }
        }
        self.tail_process.as_mut().unwrap().stdout = self.stdout.take();
        let output = self.tail_process.take().unwrap().wait_with_output();
        // We've already waited on all the exit codes. No reason to do it again on Drop.
        self.config.on_drop = OnDrop::Forget;

        if self.config.pipefail
            && let Some(err_status) = first_err_status
        {
            // We delayed returning IO errors until we finished waiting for all the processes. Now,
            // we need to return the first IO error encountered.
            let err_status = err_status?;
            let mut output = output?;
            output.status = err_status;
            Ok(output)
        } else {
            output
        }
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        match self.config.on_drop {
            OnDrop::Forget => (),
            OnDrop::Wait => {
                for process in &mut self.piped_processes {
                    let _ = process.wait();
                }
                let _ = self.tail_process.as_mut().map(|process| process.wait());
            }
            OnDrop::Kill => {
                for process in &mut self.piped_processes {
                    let _ = process.kill();
                    let _ = process.wait();
                }
                if let Some(process) = self.tail_process.as_mut()
                    && let Ok(()) = process.kill()
                {
                    let _ = process.wait();
                }
            }
        }
    }
}
