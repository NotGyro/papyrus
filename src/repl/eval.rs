use super::*;
use crate::pfh::linking::{Brw, BrwMut, NoRef};
use crate::pfh::{self, Input};
use linefeed::terminal::Terminal;
use std::path::Path;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

type HandleInputResult = (String, bool);
type EvalResult<Term, Data, Ref> = Result<Repl<Print, Term, Data, Ref>, EvalSignal>;

impl<Term: Terminal, Data> Repl<Evaluate, Term, Data, NoRef> {
    /// Evaluates the read input, compiling and executing the code and printing all line prints until a result is found.
    /// This result gets passed back as a print ready repl.
    pub fn eval(self, app_data: Data) -> EvalResult<Term, Data, NoRef> {
        map_variants(self, app_data)
    }
}

impl<Term: Terminal, Data> Repl<Evaluate, Term, Data, Brw> {
    /// Evaluates the read input, compiling and executing the code and printing all line prints until a result is found.
    /// This result gets passed back as a print ready repl.
    pub fn eval(self, app_data: &Data) -> EvalResult<Term, Data, Brw> {
        map_variants(self, app_data)
    }
}

impl<Term: Terminal, Data> Repl<Evaluate, Term, Data, BrwMut> {
    /// Evaluates the read input, compiling and executing the code and printing all line prints until a result is found.
    /// This result gets passed back as a print ready repl.
    pub fn eval(self, app_data: &mut Data) -> EvalResult<Term, Data, BrwMut> {
        map_variants(self, app_data)
    }
}

impl<Term: Terminal + 'static, Data: Send + 'static> Repl<Evaluate, Term, Data, NoRef> {
    pub fn eval_async(self, app_data: Data) -> Evaluating<Term, Data, NoRef> {
        let (tx, rx) = crossbeam::channel::bounded(1);

        std::thread::spawn(move || {
            tx.send(map_variants(self, app_data)).unwrap();
        });

        Evaluating { jh: rx }
    }
}

impl<Term: Terminal + 'static, Data: Send + Sync + 'static> Repl<Evaluate, Term, Data, Brw> {
    pub fn eval_async(self, app_data: &Arc<Data>) -> Evaluating<Term, Data, Brw> {
        let (tx, rx) = crossbeam::channel::bounded(1);

        let clone = Arc::clone(app_data);

        std::thread::spawn(move || {
            let app_data: &Data = clone.as_ref();
            tx.send(map_variants(self, app_data)).unwrap();
        });

        Evaluating { jh: rx }
    }
}

impl<Term: Terminal + 'static, Data: Send + 'static> Repl<Evaluate, Term, Data, BrwMut> {
    pub fn eval_async(self, app_data: &Arc<Mutex<Data>>) -> Evaluating<Term, Data, BrwMut> {
        use std::borrow::BorrowMut;

        let (tx, rx) = crossbeam::channel::bounded(1);

        let clone = Arc::clone(app_data);

        std::thread::spawn(move || {
            let mut lock = clone.lock().expect("failed getting lock of data");
            let app_data: &mut Data = lock.borrow_mut();
            tx.send(map_variants(self, app_data)).unwrap();
        });

        Evaluating { jh: rx }
    }
}

impl<Term: Terminal, Data, Ref> Evaluating<Term, Data, Ref> {
    pub fn completed(&self) -> bool {
        !self.jh.is_empty()
    }

    pub fn wait(self) -> EvalResult<Term, Data, Ref> {
        self.jh
            .recv()
            .expect("receiving eval result from async thread failed")
    }
}

fn map_variants<T: Terminal, D, R, Data>(
    repl: Repl<Evaluate, T, D, R>,
    app_data: Data,
) -> EvalResult<T, D, R> {
    let Repl {
        state,
        terminal,
        mut data,
        more,
        ..
    } = repl;

    // map variants into Result<HandleInputResult, EvalSignal>
    match state.result {
        InputResult::Command(cmds) => data.handle_command(&cmds, &terminal.terminal),
        InputResult::Program(input) => Ok(data.handle_program(input, &terminal.terminal, app_data)),
        InputResult::InputError(err) => Ok((err, false)),
        InputResult::Eof => Err(EvalSignal::Exit),
        _ => Ok((String::new(), false)),
    }
    .map(move |hir| {
        let (to_print, as_out) = hir;
        Repl {
            state: Print { to_print, as_out },
            terminal: terminal,
            data: data,
            more: more,
            data_mrker: PhantomData,
            ref_mrker: PhantomData,
        }
    })
}

impl ReplData {
    fn handle_command<T: Terminal>(
        &mut self,
        cmds: &str,
        terminal: &Arc<T>,
    ) -> Result<HandleInputResult, EvalSignal> {
        use cmdtree::LineResult as lr;

        // this will write to Writer(terminal)
        let tuple = match self
            .cmdtree
            .parse_line(cmds, true, &mut Writer(terminal.as_ref()))
        {
            lr::Exit => return Err(EvalSignal::Exit),
            lr::Action(res) => match res {
                CommandResult::CancelInput => ("cancelled input".to_string(), false),
            },
            _ => (String::new(), false),
        };

        Ok(tuple)
    }

    fn handle_program<T: Terminal, Data>(
        &mut self,
        input: Input,
        terminal: &Arc<T>,
        app_data: Data,
    ) -> HandleInputResult {
        let pop_input = |repl_data: &mut ReplData| {
            repl_data.get_current_file_mut().contents.pop();
        };

        let has_stmts = input.stmts.len() > 0;

        // add input file
        {
            self.get_current_file_mut().contents.push(input);
        }

        // build directory
        let res = pfh::compile::build_compile_dir(
            &self.compilation_dir,
            self.file_map.values(),
            &self.linking,
        );
        if let Err(e) = res {
            pop_input(self); // failed so don't save
            return (format!("failed to build compile directory: {}", e), false);
        }

        // format
        pfh::compile::fmt(&self.compilation_dir);

        // compile
        let lib_file = pfh::compile::compile(&self.compilation_dir, &self.linking, |line| {
            Writer(terminal.as_ref())
                .overwrite_current_console_line(&line)
                .unwrap()
        });
        Writer(terminal.as_ref())
            .overwrite_current_console_line("")
            .unwrap();
        let lib_file = match lib_file {
            Ok(f) => f,
            Err(e) => {
                pop_input(self); // failed so don't save
                return (format!("{}", e), false);
            }
        };

        if has_stmts {
            // execute
            let exec_res = {
                // Has to be done to make linux builds work
                // see:
                //		https://github.com/nagisa/rust_libloading/issues/5
                //		https://github.com/nagisa/rust_libloading/issues/41
                //		https://github.com/nagisa/rust_libloading/issues/49
                //
                // Basically the api function `dlopen` will keep loaded libraries in memory to avoid
                // continuously allocating memory. It only does not release the library when thread_local data
                // is hanging around, and it seems `println!()` is something that does this.
                // Hence to avoid not having the library not updated with a new `new()` call, a different lib
                // name is passed to the function.
                // This is very annoying as it has needless fs interactions and a growing fs footprint but
                // what can you do ¯\_(ツ)_/¯
                let lib_file = rename_lib_file(lib_file).expect("failed renaming library file");

                let redirect = self.redirect_on_execution;
                let f = self.get_current_file_mut();

                if redirect {
                    pfh::compile::exec_and_redirect(
                        &lib_file,
                        &pfh::eval_fn_name(&f.mod_path),
                        app_data,
                        OwnedWriter(Arc::clone(terminal)),
                    )
                } else {
                    pfh::compile::exec(&lib_file, &pfh::eval_fn_name(&f.mod_path), app_data)
                }
            };
            match exec_res {
                Ok(s) => ((s, true)),
                Err(e) => {
                    pop_input(self); // failed so don't save
                    (e.to_string(), false)
                }
            }
        } else {
            (String::new(), false) // do not execute if no extra statements have been added
        }
    }

    fn get_current_file_mut(&mut self) -> &mut SourceFile {
        self.file_map.get_mut(&self.current_file).expect(&format!(
            "file map does not have key: {}",
            self.current_file.display()
        ))
    }
}

fn write_exec_buffer_into_terminal<T: Terminal>(buf: &[u8], terminal: Arc<T>) {
    use std::io::Write;
    dbg!(&buf);
    eprintln!("eprint {}", String::from_utf8_lossy(&buf));
    Writer(terminal.as_ref())
        .write_all(buf)
        .expect("failed redirecting output to terminal writer");
}

/// Renames the library into a distinct file name by incrementing a counter.
/// Could fail if the number of libs grows enormous, greater than `u64`. This would mean, with
/// `u64 = 18,446,744,073,709,551,615`, even with 1KB files (prolly not) this would be
/// 18,446,744,073 TB. User will probably know something is up.
fn rename_lib_file<P: AsRef<Path>>(compiled_lib: P) -> io::Result<PathBuf> {
    let no_parent = PathBuf::new();
    let mut idx: u64 = 0;
    let parent = compiled_lib.as_ref().parent().unwrap_or(&no_parent);
    let name = |i| format!("papyrus.mem-code.lib.{}", i);
    let mut lib_path = parent.join(&name(idx));
    while lib_path.exists() {
        idx += 1;
        lib_path = parent.join(&name(idx));
    }
    std::fs::rename(&compiled_lib, &lib_path)?;
    Ok(lib_path)
}
