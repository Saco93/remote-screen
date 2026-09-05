use anyhow::{Context as _, Result, bail, ensure};
use libpulse_binding as pulse;
use pulse::{
    callbacks::ListResult,
    context::{Context, FlagSet, State},
    mainloop::standard::{IterateResult, Mainloop},
};
use std::{
    cell::RefCell,
    rc::Rc,
    thread,
    time::{Duration, Instant},
};

/// Owns only the sink created by this session. Media capture has its own connection.
pub struct Audio {
    context: Context,
    mainloop: Mainloop,
    module: Option<u32>,
    pub sink: String,
    previous_default: Option<String>,
    moved: Vec<(u32, u32)>,
}

impl Audio {
    pub fn open() -> Result<Self> {
        let mainloop = Mainloop::new().context("PulseAudio mainloop")?;
        let context = Context::new(&mainloop, "remote-screen").context("PulseAudio context")?;
        let mut this = Self {
            context,
            mainloop,
            module: None,
            sink: format!("remote_screen_{}", std::process::id()),
            previous_default: None,
            moved: vec![],
        };
        this.context.connect(None, FlagSet::NOAUTOSPAWN, None)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while this.context.get_state() != State::Ready {
            ensure!(
                !matches!(this.context.get_state(), State::Failed | State::Terminated),
                "Audio server connection failed"
            );
            this.iterate(deadline)?;
        }
        let result = Rc::new(RefCell::new(None));
        let out = result.clone();
        let _op = this.context.introspect().get_server_info(move |info| {
            *out.borrow_mut() = Some(info.default_sink_name.as_ref().map(|s| s.to_string()));
        });
        this.previous_default = this.wait(result)?;
        let result = Rc::new(RefCell::new(None));
        let out = result.clone();
        let _op = this.context.introspect().load_module(
            "module-null-sink",
            &format!(
                "sink_name={} rate=48000 channels=2 sink_properties=device.description=LG_C9",
                this.sink
            ),
            move |index| *out.borrow_mut() = Some(index),
        );
        let index = this.wait(result)?;
        ensure!(
            index != pulse::def::INVALID_INDEX,
            "Cannot create LG C9 audio output"
        );
        this.module = Some(index);
        Ok(this)
    }

    /// Route playback only once the television requests PLAY.
    pub fn route(&mut self) -> Result<()> {
        let inputs = Rc::new(RefCell::new(Vec::new()));
        let items = inputs.clone();
        let done = Rc::new(RefCell::new(None));
        let out = done.clone();
        let _op = self
            .context
            .introspect()
            .get_sink_input_info_list(move |item| match item {
                ListResult::Item(info) => items.borrow_mut().push((info.index, info.sink)),
                ListResult::End => *out.borrow_mut() = Some(true),
                ListResult::Error => *out.borrow_mut() = Some(false),
            });
        ensure!(self.wait(done)?, "Cannot enumerate playback streams");
        let result = Rc::new(RefCell::new(None));
        let out = result.clone();
        let _op = self
            .context
            .set_default_sink(&self.sink, move |ok| *out.borrow_mut() = Some(ok));
        ensure!(self.wait(result)?, "Cannot select LG audio output");
        for &(index, sink) in inputs.borrow().iter() {
            let done = Rc::new(RefCell::new(None));
            let out = done.clone();
            let _op = self.context.introspect().move_sink_input_by_name(
                index,
                &self.sink,
                Some(Box::new(move |ok| *out.borrow_mut() = Some(ok))),
            );
            if self.wait(done)? {
                self.moved.push((index, sink));
            }
        }
        Ok(())
    }

    fn iterate(&mut self, deadline: Instant) -> Result<()> {
        ensure!(
            Instant::now() < deadline,
            "Audio server operation timed out"
        );
        match self.mainloop.iterate(false) {
            IterateResult::Err(e) => bail!("Audio mainloop error: {e}"),
            IterateResult::Quit(_) => bail!("Audio mainloop quit"),
            _ => (),
        }
        thread::sleep(Duration::from_millis(2));
        Ok(())
    }

    fn wait<T>(&mut self, result: Rc<RefCell<Option<T>>>) -> Result<T> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(value) = result.borrow_mut().take() {
                return Ok(value);
            }
            self.iterate(deadline)?;
        }
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        if self.context.get_state() == State::Ready {
            for (index, sink) in std::mem::take(&mut self.moved) {
                let result = Rc::new(RefCell::new(None));
                let out = result.clone();
                let _op = self.context.introspect().move_sink_input_by_index(
                    index,
                    sink,
                    Some(Box::new(move |ok| *out.borrow_mut() = Some(ok))),
                );
                let _ = self.wait(result);
            }
            if let Some(previous) = self.previous_default.take() {
                let result = Rc::new(RefCell::new(None));
                let out = result.clone();
                let _op = self
                    .context
                    .set_default_sink(&previous, move |ok| *out.borrow_mut() = Some(ok));
                let _ = self.wait(result);
            }
            if let Some(index) = self.module.take() {
                let result = Rc::new(RefCell::new(None));
                let out = result.clone();
                let _op = self
                    .context
                    .introspect()
                    .unload_module(index, move |ok| *out.borrow_mut() = Some(ok));
                let _ = self.wait(result);
            }
        }
        self.context.disconnect();
    }
}
