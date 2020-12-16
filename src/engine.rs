extern crate hyper_timeout;
extern crate log;
extern crate serde;
extern crate serde_json;
extern crate signal_hook;
extern crate tokio;

use crate::component::{Client, Profile, Request, Response, Task, UserAgent};
use crate::macros::Spider;
use crate::macros::{MiddleWare, MiddleWareDefault, Pipeline, PipelineDefault};
use futures::future::join_all;
use log::info;
use signal_hook::flag as signal_flag;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use tokio::task;

/// number that once for a concurrent future poll
pub struct AppArg {
    pub round_req: usize,       // consume req one time
    pub round_req_min: usize,   // cache request minimal length
    pub round_req_max: usize,   // cache request maximal length
    pub round_task: usize,      // construct req from task one time
    pub round_task_min: usize,  // minimal task(profile) consumed per round
    pub round_res: usize,       // consume response once upon a time
    pub profile_min: usize,     // minimal profile number
    pub profile_max: usize,     // maximal profile number
    pub round_yield_err: usize, //consume yield_err once upon a time
    pub round_result: usize,    //consume Entity once upon a time
    pub skip_history: bool,
}

impl Default for AppArg {
    fn default() -> Self {
        AppArg {
            round_req: 100,
            round_req_min: 300,
            round_req_max: 700,
            round_task: 100,
            round_task_min: 7,
            round_res: 100,
            profile_min: 3000,
            profile_max: 10000,
            round_yield_err: 100,
            round_result: 100,
            skip_history: false,
        }
    }
}

pub struct App<Entity> {
    pub uas: Arc<Vec<UserAgent>>,
    pub task: Arc<Mutex<Vec<Task>>>,
    pub profile: Arc<Mutex<Vec<Profile>>>,
    pub req: Arc<Mutex<Vec<Request>>>,
    pub req_tmp: Arc<Mutex<Vec<Request>>>,
    pub res: Arc<Mutex<Vec<Response>>>,
    pub result: Arc<Mutex<Vec<Entity>>>,
    pub yield_err: Arc<Mutex<Vec<String>>>,
    pub fut_res: Arc<Mutex<Vec<(u64, task::JoinHandle<()>)>>>,
    pub fut_profile: Arc<Mutex<Vec<(u64, task::JoinHandle<()>)>>>,
}

impl<'a, Entity> App<Entity> {
    pub fn new() -> Self {
        App {
            uas: Arc::new(Vec::new()),
            task: Arc::new(Mutex::new(Vec::new())),
            profile: Arc::new(Mutex::new(Vec::new())),
            req: Arc::new(Mutex::new(Vec::new())),
            req_tmp: Arc::new(Mutex::new(Vec::new())),
            res: Arc::new(Mutex::new(Vec::new())),
            result: Arc::new(Mutex::new(Vec::new())),
            yield_err: Arc::new(Mutex::new(Vec::new())),
            fut_res: Arc::new(Mutex::new(Vec::new())),
            fut_profile: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub async fn run<C>(
        &'a mut self,
        args: Option<AppArg>,
        spd: &'static dyn Spider<Entity>,
        mware: Option<&'a dyn MiddleWare<Entity>>,
        pline: Option<&'a dyn Pipeline<Entity, C>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // signal handling initial
        let term = Arc::new(AtomicUsize::new(0));
        const SIGINT: usize = signal_hook::SIGINT as usize;
        signal_flag::register_usize(signal_hook::SIGINT, Arc::clone(&term), SIGINT).unwrap();

        let args = match args {
            Some(para) => para,
            None => AppArg::default(),
        };
        let default_pl = PipelineDefault::new();
        let default_mw = MiddleWareDefault::new();
        spd.open_spider(self);
        //skip the history and start new fields to staart with, some Profile required
        if args.skip_history {
            info!("does not skip the history.");
            let uri = spd.entry_profile().unwrap();
            let uas = self.uas.clone();
            Profile::exec_all(spd, self.profile.clone(), uri, 7, uas).await;
            let tasks = spd.entry_task().unwrap();
            self.task.lock().unwrap().extend(tasks);
        }

        loop {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();

            match term.load(Ordering::Relaxed) {
                SIGINT => {
                    // receive the Ctrl+c signal
                    // by default  request  task profile and result yield err are going to stroed into
                    // file

                    //finish remaining futures
                    let mut v = Vec::new();
                    while let Some(res) = self.fut_res.lock().unwrap().pop() {
                        //res.await;
                        v.push(res.1);
                    }
                    join_all(v).await;

                    // dispath them
                    match mware {
                        Some(ware) => Response::parse_all(self, 99999999, spd, ware),
                        None => Response::parse_all(self, 99999999, spd, &default_mw),
                    }

                    //store them
                    match pline {
                        Some(pl) => {
                            pl.process_item(&mut self.result);
                            pl.process_yielderr(&mut self.yield_err);
                        }
                        None => {
                            default_pl.process_item(&mut self.result);
                            default_pl.process_yielderr(&mut self.yield_err);
                        }
                    }
                    spd.close_spider(self);
                }

                0 => {
                    // if all task request and other things are done the quit
                    if self.yield_err.lock().unwrap().is_empty()
                        && self.req.lock().unwrap().is_empty()
                        && self.task.lock().unwrap().is_empty()
                        && self.result.lock().unwrap().is_empty()
                        && self.profile.lock().unwrap().is_empty()
                    {
                        info!("All work is Done. exit gracefully");
                        break;
                    }

                    // consume valid request in cbase_reqs_tmp
                    // if not enough take them from cbase_reqs
                    if self.req_tmp.lock().unwrap().len() <= args.round_req_min {
                        // cached request is not enough
                        for _ in 0..self.req.lock().unwrap().len() {
                            let req = self.req.lock().unwrap().pop().unwrap();
                            if req.able <= now {
                                // put the request into cbase_req_tmp
                                self.req_tmp.lock().unwrap().push(req);
                            }

                            if self.req_tmp.lock().unwrap().len() > args.round_req_max {
                                break;
                            }
                        }
                    }

                    //take req out to finish
                    let mut futs = Vec::new();
                    let len = args.round_req.min(self.req_tmp.lock().unwrap().len());
                    vec![0; len].iter().for_each(|_| {
                        let req = self.req_tmp.lock().unwrap().pop().unwrap();
                        futs.push(req);
                    });
                    let tbase_res = self.res.clone();
                    let john = task::spawn(async move {
                        Client::exec_all(futs, tbase_res).await;
                    });
                    self.fut_res.lock().unwrap().push((now, john));

                    // before we construct request check profile first
                    let less = self.profile.lock().unwrap().len() <= args.profile_min;
                    let exceed = !less
                        && self.profile.lock().unwrap().len() <= args.profile_max
                        && now % 3 == 1;
                    if exceed || less {
                        let uas = self.uas.clone();
                        let uri = spd.entry_profile().unwrap();
                        let pfile = self.profile.clone();
                        let johp = task::spawn(async move {
                            Profile::exec_all(spd, pfile, uri, 7, uas).await;
                        });
                        self.fut_profile.lock().unwrap().push((now, johp));
                    }

                    // parse response
                    //extract the parseResult
                    match mware {
                        Some(ware) => Response::parse_all(self, args.round_res, spd, ware),
                        None => Response::parse_all(self, args.round_res, spd, &default_mw),
                    }

                    //pipeline put out yield_parse_err and Entity
                    if self.yield_err.lock().unwrap().len() > args.round_yield_err {
                        match pline {
                            Some(pl) => pl.process_yielderr(&mut self.yield_err),
                            None => {}
                        }
                    }
                    if self.result.lock().unwrap().len() > args.round_result {
                        match pline {
                            Some(pl) => pl.process_item(&mut self.result),
                            None => {}
                        }
                    }

                    // count for profiles length if not more than round_task_min
                    if args.round_task_min > self.profile.lock().unwrap().len() {
                        // not enough profile to construct request
                        // await the spawned task doe
                        let jh = self.fut_profile.lock().unwrap().pop().unwrap();
                        jh.1.await.unwrap();
                    }

                    // construct request
                    Request::gen(self, args.round_task);

                    //join the older tokio-task
                    Client::join(self.fut_res.clone(), self.fut_profile.clone()).await;
                }

                _ => unreachable!(),
            }
        }

        Ok(())
    }
}
