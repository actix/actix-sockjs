#![allow(unused_imports)]
use std::sync::Arc;
use std::marker::PhantomData;

use actix::*;
use actix_web::*;
use regex::Regex;
use serde_json;
use bytes::BytesMut;
use futures::future::{ok, Future, Either};
use percent_encoding::percent_decode;

use protocol::{Frame, CloseCode};
use utils::SockjsHeaders;
use session::{Message, Session};
use manager::{Broadcast, Record, SessionManager, SessionMessage};

use super::{MAXSIZE, Transport, SendResult};


pub struct JSONPolling<S, SM>
    where S: Session, SM: SessionManager<S>,
{
    s: PhantomData<S>,
    sm: PhantomData<SM>,
    rec: Option<Record>,
    callback: String,
}

// Http actor implementation
impl<S, SM> Actor for JSONPolling<S, SM>
    where S: Session, SM: SessionManager<S>
{
    type Context = HttpContext<Self, SyncAddress<SM>>;

    fn stopping(&mut self, ctx: &mut Self::Context) {
        self.stop(ctx);
    }
}

impl<S, SM>  JSONPolling<S, SM>
    where S: Session, SM: SessionManager<S>
{
    fn write(&self, s: &str, ctx: &mut HttpContext<Self, SyncAddress<SM>>) {
        ctx.write(format!("/**/{}({});\r\n",
                          self.callback, serde_json::to_string(s).unwrap()))
    }
}

// Transport implementation
impl<S, SM> Transport<S, SM> for JSONPolling<S, SM>
    where S: Session, SM: SessionManager<S>,
{
    fn send(&mut self, ctx: &mut Self::Context, msg: &Frame, record: &mut Record)
            -> SendResult
    {
        match *msg {
            Frame::Heartbeat => {
                self.write("h", ctx);
            },
            Frame::Message(ref s) => {
                self.write(&format!("a[{:?}]", s), ctx);
            }
            Frame::MessageVec(ref s) => {
                self.write(&format!("a{}", s), ctx);
            }
            Frame::MessageBlob(_) => {
                unimplemented!()
            }
            Frame::Open => {
                self.write("o", ctx);
            },
            Frame::Close(code) => {
                record.close();
                let blob = format!("c[{},{:?}]", code.num(), code.reason());
                self.write(&blob, ctx);
            }
        };
        ctx.write_eof();
        SendResult::Stop
    }

    fn send_heartbeat(&mut self, ctx: &mut Self::Context)
    {
        self.write("h\n", ctx);
        ctx.write_eof();
    }

    fn send_close(&mut self, ctx: &mut Self::Context, code: CloseCode)
    {
        self.write(&format!("c[{},{:?}]", code.num(), code.reason()), ctx);
        ctx.write_eof();
    }

    fn session_record(&mut self) -> &mut Option<Record> {
        &mut self.rec
    }
}

impl<S, SM> JSONPolling<S, SM>
    where S: Session, SM: SessionManager<S>,
{
    pub fn init(req: HttpRequest<SyncAddress<SM>>) -> Result<HttpResponse>
    {
        lazy_static! {
            static ref CHECK: Regex = Regex::new(r"^[a-zA-Z0-9_\.]+$").unwrap();
        }

        if *req.method() != Method::GET {
            return Ok(httpcodes::HTTPNotFound.into())
        }

        if let Some(callback) = req.query().get("c").map(|s| s.to_owned()) {
            if !CHECK.is_match(&callback) {
                return Ok(
                    httpcodes::HTTPInternalServerError
                        .with_body("invalid \"callback\" parameter"))
            }

            let session = req.match_info().get("session").unwrap().to_owned();
            let mut resp = httpcodes::HTTPOk
                .build()
                .content_type("application/javascript; charset=UTF-8")
                .force_close()
                .sockjs_no_cache()
                .sockjs_session_cookie(&req)
                .sockjs_cors_headers(req.headers())
                .take();

            let mut ctx = HttpContext::from_request(req);
            let mut transport = JSONPolling{s: PhantomData,
                                            sm: PhantomData,
                                            rec: None,
                                            callback: callback};
            // init transport
            transport.init_transport(session, &mut ctx);

            Ok(resp.body(ctx.actor(transport))?)
        } else {
            Ok(httpcodes::HTTPInternalServerError.with_body("\"callback\" parameter required"))
        }
    }
}

impl<S, SM> StreamHandler<Frame> for JSONPolling<S, SM>
    where S: Session, SM: SessionManager<S> {}

impl<S, SM> Handler<Frame> for JSONPolling<S, SM>
    where S: Session, SM: SessionManager<S>,
{
    fn handle(&mut self, msg: Frame, ctx: &mut Self::Context) -> Response<Self, Frame> {
        if let Some(mut rec) = self.rec.take() {
            self.send(ctx, &msg, &mut rec);
            self.rec = Some(rec);
        } else if let Some(ref mut rec) = self.rec {
            rec.add(msg);
        }
        Self::empty()
    }
}

impl<S, SM> Handler<Broadcast> for JSONPolling<S, SM>
    where S: Session, SM: SessionManager<S>,
{
    fn handle(&mut self, msg: Broadcast, ctx: &mut Self::Context)
              -> Response<Self, Broadcast>
    {
        if let Some(mut rec) = self.rec.take() {
            self.send(ctx, &msg.msg, &mut rec);
            self.rec = Some(rec);
        } else if let Some(ref mut rec) = self.rec {
            rec.add(msg);
        }
        Self::empty()
    }
}

#[allow(non_snake_case)]
pub fn JSONPollingSend<S, SM>(req: HttpRequest<SyncAddress<SM>>)
                              -> Either<HttpResponse, Box<Future<Item=HttpResponse, Error=Error>>>
    where S: Session, SM: SessionManager<S>,
{
    if *req.method() != Method::POST {
        Either::A(httpcodes::HTTPBadRequest.with_reason("Method is not allowed"))
    } else {
        Either::B(read(req))
    }

}

pub fn read<S, SM>(req: HttpRequest<SyncAddress<SM>>)
                   -> Box<Future<Item=HttpResponse, Error=Error>>
    where S: Session, SM: SessionManager<S>
{
    let sid = req.match_info().get("session").unwrap().to_owned();

    Box::new(
        req.body().limit(MAXSIZE)
            .map_err(|e| Error::from(error::ErrorBadRequest(e)))
            .and_then(move |buf| {
                let sid = Arc::new(sid);

                // empty message
                if buf.is_empty() {
                    return Either::A(
                        ok(httpcodes::HTTPInternalServerError.with_body("Payload expected.")))
                } else {
                    // deserialize json
                    let mut msgs: Vec<String> =
                        if req.content_type() == "application/x-www-form-urlencoded"
                    {
                        if buf.len() <= 2 || &buf[..2] != b"d=" {
                            return Either::A(
                                ok(httpcodes::HTTPInternalServerError
                                   .with_body("Payload expected.")));
                        }

                        if let Ok(data) = percent_decode(&buf[2..]).decode_utf8() {
                            match serde_json::from_slice(data.as_ref().as_ref()) {
                                Ok(msgs) => msgs,
                                Err(_) => {
                                    return Either::A(
                                        ok(httpcodes::HTTPInternalServerError.with_body(
                                            "Broken JSON encoding.")));
                                }
                            }
                        } else {
                            return Either::A(
                                ok(httpcodes::HTTPInternalServerError
                                   .with_body("Payload expected.")));
                        }
                    } else {
                        match serde_json::from_slice(&buf) {
                            Ok(msgs) => msgs,
                            Err(_) => {
                                return Either::A(
                                    ok(httpcodes::HTTPInternalServerError.with_body(
                                        "Broken JSON encoding.")));
                            }
                        }
                    };

                    // do nothing
                    if msgs.is_empty() {
                        return Either::A(
                            ok(httpcodes::HTTPOk.build()
                               .content_type("text/plain; charset=UTF-8")
                               .sockjs_no_cache()
                               .sockjs_session_cookie(&req)
                               .body("ok").unwrap()));
                    } else {
                        let last = msgs.pop().unwrap();
                        for msg in msgs {
                            req.state().send(
                                SessionMessage {
                                    sid: sid.clone(),
                                    msg: Message(msg)});
                        }

                        return Either::B(
                            req.state().call_fut(SessionMessage { sid: sid.clone(),
                                                                  msg: Message(last)})
                                .from_err()
                                .and_then(move |res| {
                                    match res {
                                        Ok(_) => {
                                            Ok(httpcodes::HTTPOk
                                               .build()
                                               .content_type("text/plain; charset=UTF-8")
                                               .sockjs_no_cache()
                                               .sockjs_session_cookie(&req)
                                               .body("ok")
                                               .unwrap())
                                        },
                                        Err(e) => Err(Error::from(error::ErrorNotFound(e))),
                                    }
                                }))
                    }
                }
            }))
}
