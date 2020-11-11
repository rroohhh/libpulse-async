#![feature(drain_filter)]
// use std::io;

// use pulsectl::controllers::{SourceController, SinkController};
// use pulsectl::controllers::{DeviceControl, AppControl};

// fn main() {
//     // create handler that calls functions on playback devices and apps
//     let mut handler = SourceController::create();
//     let devices = handler
//         .list_devices()
//         .expect("Could not get list of source applications");
//     println!("Source Devices");
//     for dev in devices.clone() {
//         println!(
//             "[{}] {}, [Volume: {}]",
//             dev.index,
//             dev.description.as_ref().unwrap(),
//             dev.volume.print()
//         );
//     }

//     let mut handler = SinkController::create();
//     let apps = handler
//         .list_applications()
//         .expect("Could not get list of source applications");
//     println!("Source Applications");
//     for app in apps.clone() {
//         println!(
//             "[{}] {}, [Volume: {}]",
//             app.index,
//             app.name.as_ref().unwrap(),
//             app.volume.print()
//         );
//     }
// }

extern crate libpulse_binding as pulse;

use async_std;
use pulse::context::Context as StandardContext;
use pulse::def::Retval;
use pulse::mainloop::standard::IterateResult;
use pulse::mainloop::standard::Mainloop as StandardMainloop;
use pulse::proplist::Proplist;
use pulse::stream::Stream;
use std::cell::RefCell;
use std::cell::UnsafeCell;
use std::collections::LinkedList;
use std::future::Future;
use std::ops::Deref;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc,
    mpsc::Sender,
    Arc, Mutex, RwLock,
};
use std::task::{Poll, Waker};
use std::thread;
use std::thread::JoinHandle;

enum PATaskValueResult<T> {
    Done(T),
    Waiting,
}

enum PATaskResult {
    Done,
    Waiting,
}

struct PAFuture<T> {
    done: Arc<AtomicBool>,
    outdata: Arc<Mutex<Option<T>>>,
    has_waker: Arc<AtomicBool>,
    waker: Arc<RwLock<Option<Waker>>>,
}

struct PAAsyncMainloop {
    threadHandle: JoinHandle<()>,
    stop: Arc<AtomicBool>,
    tasks: Sender<Box<dyn PAAsyncMainloopRunnable + Send>>,
    mainloop: Mainloop,
}

struct PAAsyncMainloopTask<T, F> {
    done: Arc<AtomicBool>,
    outdata: Arc<Mutex<Option<T>>>,
    has_waker: Arc<AtomicBool>,
    waker: Arc<RwLock<Option<Waker>>>,
    task: F,
}

impl<T, F> PAAsyncMainloopTask<T, F>
where
    F: FnMut() -> PATaskValueResult<T>,
{
    fn new(task: F) -> Self {
        PAAsyncMainloopTask {
            done: Arc::new(AtomicBool::new(false)),
            outdata: Arc::new(Mutex::new(None)),
            has_waker: Arc::new(AtomicBool::new(false)),
            waker: Arc::new(RwLock::new(None)),
            task,
        }
    }

    fn get_future(&self) -> PAFuture<T> {
        PAFuture {
            done: self.done.clone(),
            outdata: self.outdata.clone(),
            has_waker: self.has_waker.clone(),
            waker: self.waker.clone(),
        }
    }
}

trait PAAsyncMainloopRunnable {
    fn run(&mut self) -> PATaskResult;
}

impl<T, F> PAAsyncMainloopRunnable for PAAsyncMainloopTask<T, F>
where
    F: FnMut() -> PATaskValueResult<T>,
{
    fn run(&mut self) -> PATaskResult {
        assert!(!self.done.load(Ordering::SeqCst));

        if let PATaskValueResult::Done(result) = (self.task)() {
            *self.outdata.lock().unwrap() = Some(result);

            self.done.store(true, Ordering::SeqCst);

            if self.has_waker.load(Ordering::SeqCst) {
                self.waker.read().unwrap().as_ref().unwrap().wake_by_ref();
            }

            PATaskResult::Done
        } else {
            PATaskResult::Waiting
        }
    }
}

struct PAAsyncHandle<T> {
    inner: Arc<UnsafeCell<T>>,
}

// Should be safe as long as you only send them to mainloop tasks
// (they are only executed serially there and only in that thread)
unsafe impl<T> Send for PAAsyncHandle<T> {}
// unsafe impl<T> Sync for PAAsyncHandle<T> {}

impl<T> PAAsyncHandle<T> {
    fn new(val: T) -> Self {
        PAAsyncHandle {
            inner: Arc::new(UnsafeCell::new(val)),
        }
    }

    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }

    // These should be safe as long as you only use them in mainloop tasks
    unsafe fn borrow<'a>(&'a self) -> &'a T {
        &*self.inner.get()
    }

    unsafe fn borrow_mut<'a>(&'a mut self) -> &'a mut T {
        &mut *self.inner.get()
    }
}

type Mainloop = PAAsyncHandle<StandardMainloop>;

struct Context(PAAsyncHandle<StandardContext>);

impl Context {
    async fn new_with_proplist(
        mainloop: PAAsyncMainloop,
        name: &'static str,
        proplist: Proplist,
    ) -> Option<Context> {
        let used_mainloop = mainloop.mainloop.clone();

        mainloop
            .submit(move || {
                PATaskValueResult::Done(StandardContext::new_with_proplist(
                    unsafe { used_mainloop.borrow() },
                    name,
                    &proplist,
                ))
            })
            .await
            .map(|v| Context(PAAsyncHandle::new(v)))
    }
}

impl PAAsyncMainloop {
    fn new() -> Option<Self> {
        let (tasks, tasksReceiver) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopForRunner = stop.clone();

        if let Some(mainloop) = StandardMainloop::new() {
            let mut mainloop = PAAsyncHandle::new(mainloop);
            let mut mainloopForRunner = mainloop.clone();

            let threadHandle = thread::spawn(move || {
                let mut tasks = LinkedList::<Box<dyn PAAsyncMainloopRunnable + Send>>::new();
                let mut mainloop = mainloopForRunner;

                while !stopForRunner.load(Ordering::SeqCst) {
                    match unsafe { mainloop.borrow_mut() }.iterate(false) {
                        IterateResult::Quit(_) | IterateResult::Err(_) => {
                            eprintln!("Iterate state was not success, quitting...");
                            return;
                        }
                        IterateResult::Success(_) => {}
                    }

                    while let Ok(task) = tasksReceiver.try_recv() {
                        tasks.push_back(task);
                    }

                    tasks.drain_filter(|task| {
                        matches!(task.run(), PATaskResult::Done)
                    });
                }

                unsafe { mainloop.borrow_mut() }.quit(Retval(0));
            });

            Some(PAAsyncMainloop {
                threadHandle,
                stop,
                tasks,
                mainloop,
            })
        } else {
            None
        }
    }

    fn submit<T: Send + 'static, F: FnMut() -> PATaskValueResult<T> + Send + 'static>(
        &self,
        task: F,
    ) -> PAFuture<T> {
        let task = PAAsyncMainloopTask::new(task);
        let future = task.get_future();
        self.tasks.send(Box::new(task));
        future
    }
}

impl<T> Future for PAFuture<T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context) -> Poll<Self::Output> {
        *self.waker.write().unwrap() = Some(cx.waker().clone());
        self.has_waker.store(true, Ordering::SeqCst);

        if !self.done.load(Ordering::SeqCst) {
            Poll::Pending
        } else {
            Poll::Ready(self.outdata.lock().unwrap().take().unwrap())
        }
    }
}

#[async_std::main]
async fn main() {
    let mut proplist = Proplist::new().unwrap();
    proplist
        .set_str(pulse::proplist::properties::APPLICATION_NAME, "FooApp")
        .unwrap();

    let mut mainloop = PAAsyncMainloop::new().expect("Failed to create mainloop");

    let mut context = Context::new_with_proplist(mainloop, "FooAppContext", proplist)
        .await
        .expect("Failed to create new context");


}

/*
fn main() {
    let spec = pulse::sample::Spec {
        format: pulse::sample::SAMPLE_S16NE,
        channels: 2,
        rate: 44100,
    };
    assert!(spec.is_valid());

    let mut proplist = Proplist::new().unwrap();
    proplist
        .set_str(pulse::proplist::properties::APPLICATION_NAME, "FooApp")
        .unwrap();

    let mut mainloop = Rc::new(RefCell::new(
        Mainloop::new().expect("Failed to create mainloop"),
    ));

    let mut context = Rc::new(RefCell::new(
        Context::new_with_proplist(mainloop.borrow().deref(), "FooAppContext", &proplist)
            .expect("Failed to create new context"),
    ));

    context
        .borrow_mut()
        .connect(None, pulse::context::flags::NOFLAGS, None)
        .expect("Failed to connect context");

    // Wait for context to be ready
    loop {
        match mainloop.borrow_mut().iterate(false) {
            IterateResult::Quit(_) | IterateResult::Err(_) => {
                eprintln!("Iterate state was not success, quitting...");
                return;
            }
            IterateResult::Success(_) => {}
        }
        match context.borrow().get_state() {
            pulse::context::State::Ready => {
                break;
            }
            pulse::context::State::Failed | pulse::context::State::Terminated => {
                eprintln!("Context state failed/terminated, quitting...");
                return;
            }
            _ => {}
        }
    }

    let mut stream = Rc::new(RefCell::new(
        Stream::new(&mut context.borrow_mut(), "Music", &spec, None)
            .expect("Failed to create new stream"),
    ));

    stream
        .borrow_mut()
        .connect_playback(None, None, pulse::stream::flags::START_CORKED, None, None)
        .expect("Failed to connect playback");

    // Wait for stream to be ready
    loop {
        match mainloop.borrow_mut().iterate(false) {
            IterateResult::Quit(_) | IterateResult::Err(_) => {
                eprintln!("Iterate state was not success, quitting...");
                return;
            }
            IterateResult::Success(_) => {}
        }
        match stream.borrow().get_state() {
            pulse::stream::State::Ready => {
                break;
            }
            pulse::stream::State::Failed | pulse::stream::State::Terminated => {
                eprintln!("Stream state failed/terminated, quitting...");
                return;
            }
            _ => {}
        }
    }

    // Our main loop
    let drained = Rc::new(atomic::AtomicBool::new(false));
    loop {
        match mainloop.borrow_mut().iterate(false) {
            IterateResult::Quit(_) | IterateResult::Err(_) => {
                eprintln!("Iterate state was not success, quitting...");
                return;
            }
            IterateResult::Success(_) => {}
        }

        // Write some data with stream.write()

        if stream.borrow().is_corked().unwrap() {
            stream.borrow_mut().uncork(None);
        }

        // Wait for our data to be played
        let _o = {
            let drain_state_ref = Rc::clone(&drained);
            stream
                .borrow_mut()
                .drain(Some(Box::new(move |_success: bool| {
                    drain_state_ref.store(true, atomic::Ordering::Relaxed);
                })))
        };
        while !drained.compare_and_swap(true, false, atomic::Ordering::Relaxed) {
            match mainloop.borrow_mut().iterate(false) {
                IterateResult::Quit(_) | IterateResult::Err(_) => {
                    eprintln!("Iterate state was not success, quitting...");
                    return;
                }
                IterateResult::Success(_) => {}
            }
        }

        // Remember to break out of the loop once done writing all data (or whatever).
    }

    // Clean shutdown
    mainloop.borrow_mut().quit(Retval(0)); // uncertain whether this is necessary
    stream.borrow_mut().disconnect().unwrap();
}

*/
