﻿/*
    Copyright 2014-2015 Zumero, LLC

    Licensed under the Apache License, Version 2.0 (the "License");
    you may not use this file except in compliance with the License.
    You may obtain a copy of the License at

        http://www.apache.org/licenses/LICENSE-2.0

    Unless required by applicable law or agreed to in writing, software
    distributed under the License is distributed on an "AS IS" BASIS,
    WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
    See the License for the specific language governing permissions and
    limitations under the License.
*/

#![feature(core)]
#![feature(collections)]
#![feature(box_syntax)]
#![feature(convert)]
#![feature(collections_drain)]
#![feature(associated_consts)]
#![feature(vec_push_all)]
#![feature(clone_from_slice)]
#![feature(drain)]
#![feature(iter_arith)]
#![feature(result_expect)]

// TODO turn the following warnings back on later
#![allow(non_snake_case)]
#![allow(non_camel_case_types)]

extern crate misc;

use misc::endian;
use misc::bufndx;
use misc::varint;

extern crate bson;
use bson::BsonValue;

extern crate elmo;

extern crate elmo_sqlite3;

use std::io;
use std::io::Seek;
use std::io::Read;
use std::io::Write;
use std::io::SeekFrom;
use std::cmp::Ordering;
use std::fs::File;
use std::fs::OpenOptions;
use std::collections::HashMap;
use std::collections::HashSet;

#[derive(Debug)]
enum Error {
    // TODO remove Misc
    Misc(&'static str),

    // TODO more detail within CorruptFile
    CorruptFile(&'static str),

    Bson(bson::BsonError),
    Io(std::io::Error),
    Utf8(std::str::Utf8Error),
    Whatever(Box<std::error::Error>),

    CursorNotValid,
    InvalidPageNumber,
    InvalidPageType,
    RootPageNotInSegmentBlockList,
    Poisoned,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match *self {
            Error::Bson(ref err) => write!(f, "bson error: {}", err),
            Error::Io(ref err) => write!(f, "IO error: {}", err),
            Error::Utf8(ref err) => write!(f, "Utf8 error: {}", err),
            Error::Misc(s) => write!(f, "Misc error: {}", s),
            Error::CorruptFile(s) => write!(f, "Corrupt file: {}", s),
            Error::Poisoned => write!(f, "Poisoned"),
            Error::CursorNotValid => write!(f, "Cursor not valid"),
            Error::InvalidPageNumber => write!(f, "Invalid page number"),
            Error::InvalidPageType => write!(f, "Invalid page type"),
            Error::RootPageNotInSegmentBlockList => write!(f, "Root page not in segment block list"),
            Error::Whatever(ref err) => write!(f, "Other error: {}", err),
        }
    }
}

impl std::error::Error for Error {
    fn description(&self) -> &str {
        match *self {
            Error::Bson(ref err) => std::error::Error::description(err),
            Error::Io(ref err) => std::error::Error::description(err),
            Error::Utf8(ref err) => std::error::Error::description(err),
            Error::Misc(s) => s,
            Error::CorruptFile(s) => s,
            Error::Poisoned => "poisoned",
            Error::CursorNotValid => "cursor not valid",
            Error::InvalidPageNumber => "invalid page number",
            Error::InvalidPageType => "invalid page type",
            Error::RootPageNotInSegmentBlockList => "Root page not in segment block list",
            Error::Whatever(ref err) => std::error::Error::description(&**err),
        }
    }

    // TODO cause
}

impl From<bson::BsonError> for Error {
    fn from(err: bson::BsonError) -> Error {
        Error::Bson(err)
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Error {
        Error::Io(err)
    }
}

impl From<std::str::Utf8Error> for Error {
    fn from(err: std::str::Utf8Error) -> Error {
        Error::Utf8(err)
    }
}

impl<T> From<std::sync::PoisonError<T>> for Error {
    fn from(_err: std::sync::PoisonError<T>) -> Error {
        Error::Poisoned
    }
}

// TODO why is 'static needed here?  Doesn't this take ownership?
fn wrap_err<E: std::error::Error + 'static>(err: E) -> Error {
    Error::Whatever(box err)
}

pub type Result<T> = std::result::Result<T, Error>;

struct BsonMsgReply {
    r_requestID : i32,
    r_responseTo : i32,
    r_responseFlags : i32,
    r_cursorID : i64,
    r_startingFrom : i32,
    r_documents : Vec<BsonValue>,
}

struct BsonMsgQuery {
    q_requestID : i32,
    q_flags : i32,
    q_fullCollectionName : String,
    q_numberToSkip : i32,
    q_numberToReturn : i32,
    q_query : BsonValue,
    q_returnFieldsSelector : Option<BsonValue>,
}

struct BsonMsgGetMore {
    m_requestID : i32,
    m_fullCollectionName : String,
    m_numberToReturn : i32,
    m_cursorID : i64,
}

struct BsonMsgKillCursors {
    k_requestID : i32,
    k_cursorIDs : Vec<i64>,
}

enum BsonClientMsg {
    BsonMsgQuery(BsonMsgQuery),
    BsonMsgGetMore(BsonMsgGetMore),
    BsonMsgKillCursors(BsonMsgKillCursors),
}

impl BsonMsgReply {
    fn encodeReply(&self) -> Box<[u8]> {
        let mut w = Vec::new();
        // length placeholder
        w.push_all(&[0u8; 4]);
        w.push_all(&endian::i32_to_bytes_le(self.r_requestID));
        w.push_all(&endian::i32_to_bytes_le(self.r_responseTo));
        w.push_all(&endian::u32_to_bytes_le(1u32)); 
        w.push_all(&endian::i32_to_bytes_le(self.r_responseFlags));
        w.push_all(&endian::i64_to_bytes_le(self.r_cursorID));
        w.push_all(&endian::i32_to_bytes_le(self.r_startingFrom));
        w.push_all(&endian::u32_to_bytes_le(self.r_documents.len() as u32));
        for doc in &self.r_documents {
            doc.to_bson(&mut w);
        }
        misc::bytes::copy_into(&endian::u32_to_bytes_be(w.len() as u32), &mut w[0 .. 4]);
        w.into_boxed_slice()
    }
}

fn parseMessageFromClient(ba: &[u8]) -> Result<BsonClientMsg> {
    let mut i = 0;
    let (messageLength,requestID,resonseTo,opCode) = slurp_header(ba, &mut i);
    match opCode {
        2004 => {
            let flags = bufndx::slurp_i32_le(ba, &mut i);
            let fullCollectionName = try!(bufndx::slurp_cstring(ba, &mut i));
            let numberToSkip = bufndx::slurp_i32_le(ba, &mut i);
            let numberToReturn = bufndx::slurp_i32_le(ba, &mut i);
            let query = try!(bson::slurp_document(ba, &mut i));
            let returnFieldsSelector = if i < ba.len() { Some(try!(bson::slurp_document(ba, &mut i))) } else { None };

            let msg = BsonMsgQuery {
                q_requestID: requestID,
                q_flags: flags,
                q_fullCollectionName: fullCollectionName,
                q_numberToSkip: numberToSkip,
                q_numberToReturn: numberToReturn,
                q_query: query,
                q_returnFieldsSelector: returnFieldsSelector,
            };
            Ok(BsonClientMsg::BsonMsgQuery(msg))
        },

        2005 => {
            let flags = bufndx::slurp_i32_le(ba, &mut i);
            let fullCollectionName = try!(bufndx::slurp_cstring(ba, &mut i));
            let numberToReturn = bufndx::slurp_i32_le(ba, &mut i);
            let cursorID = bufndx::slurp_i64_le(ba, &mut i);

            let msg = BsonMsgGetMore {
                m_requestID: requestID,
                m_fullCollectionName: fullCollectionName,
                m_numberToReturn: numberToReturn,
                m_cursorID: cursorID,
            };
            Ok(BsonClientMsg::BsonMsgGetMore(msg))
        },

        2007 => {
            let flags = bufndx::slurp_i32_le(ba, &mut i);
            let numberOfCursorIDs = bufndx::slurp_i32_le(ba, &mut i);
            let mut cursorIDs = Vec::new();
            for _ in 0 .. numberOfCursorIDs {
                cursorIDs.push(bufndx::slurp_i64_le(ba, &mut i));
            }

            let msg = BsonMsgKillCursors {
                k_requestID: requestID,
                k_cursorIDs: cursorIDs,
            };
            Ok(BsonClientMsg::BsonMsgKillCursors(msg))
        },

        _ => {
            Err(Error::CorruptFile("unknown message opcode TODO"))
        },
    }
}

// TODO do these really need to be signed?
fn slurp_header(ba: &[u8], i: &mut usize) -> (i32,i32,i32,i32) {
    let messageLength = bufndx::slurp_i32_le(ba, i);
    let requestID = bufndx::slurp_i32_le(ba, i);
    let responseTo = bufndx::slurp_i32_le(ba, i);
    let opCode = bufndx::slurp_i32_le(ba, i);
    let v = (messageLength,requestID,responseTo,opCode);
    v
}

fn readMessage(stream: &mut Read) -> Result<Box<[u8]>> {
    let ba = try!(misc::io::read_4(stream));
    let messageLength = endian::u32_from_bytes_le(ba) as usize;
    let mut msg = vec![0; messageLength]; 
    misc::bytes::copy_into(&ba, &mut msg[0 .. 4]);
    let got = try!(misc::io::read_fully(stream, &mut msg[4 .. messageLength]));
    if got != messageLength - 4 {
        return Err(Error::CorruptFile("end of file at the wrong time"));
    }
    Ok(msg.into_boxed_slice())
}

fn createReply(reqID: i32, docs: Vec<BsonValue>, crsrID: i64) -> BsonMsgReply {
    let msg = BsonMsgReply {
        r_requestID: 0,
        r_responseTo: reqID,
        r_responseFlags: 0,
        r_cursorID: crsrID,
        r_startingFrom: 0,
        // TODO
        r_documents: docs,
    };
    msg
}

fn reply_err(requestID: i32, err: Error) -> BsonMsgReply {
    let mut pairs = Vec::new();
    // TODO stack trace was nice here
    //pairs.push(("errmsg", BString("exception: " + errmsg)));
    //pairs.push(("code", BInt32(code)));
    pairs.push((String::from_str("ok"), BsonValue::BInt32(0)));
    let doc = BsonValue::BDocument(pairs);
    createReply(requestID, vec![doc], 0)
}

struct Server {
    conn: elmo::Connection,
}

impl Server {

    fn doInsert(&self, db: &str, coll: &str, s: &Vec<BsonValue>) -> Result<(Vec<BsonValue>,Vec<BsonValue>)> {
        /* TODO
        let results = crud.insert(db, coll, s);
        let writeErrors =
            results
            |> Array.filter (fun (_,err) ->
                match err with
                | Some _ -> true
                | None -> false
            )
            |> Array.mapi (fun i (_,err) ->
                // TODO use of Option.get is bad, but we just filtered all the Some values above
                BDocument [| ("index",BInt32 i); ("errmsg",Option.get err |> BString) |]
            )
        (results,writeErrors)
        */
        Ok((Vec::new(), Vec::new()))
    }

    fn reply_Insert(&self, clientMsg: &BsonMsgQuery, db: &str) -> Result<BsonMsgReply> {
        let q = &clientMsg.q_query;
        let coll = try!(try!(q.getValueForKey("insert")).getString());
        let docs = try!(try!(q.getValueForKey("documents")).getArray());
        // TODO ordered
        let (results,writeErrors) = try!(self.doInsert(db, coll, docs));
        let mut pairs = Vec::new();
        pairs.push((String::from_str("n"), BsonValue::BInt32((results.len() - writeErrors.len()) as i32)));
        //pairs.Add("lastOp", BTimeStamp 0L) // TODO
        if writeErrors.len() > 0 {
            pairs.push((String::from_str("writeErrors"), BsonValue::BArray(writeErrors)));
        }
        // TODO electionId?
        pairs.push((String::from_str("ok"), BsonValue::BDouble(1.0)));
        let doc = BsonValue::BDocument(pairs);
        Ok(createReply(clientMsg.q_requestID, vec![doc], 0))
    }

    // TODO this layer of the code should not be using Error

    fn reply_cmd(&self, clientMsg: &BsonMsgQuery, db: &str) -> Result<BsonMsgReply> {
        match clientMsg.q_query {
            BsonValue::BDocument(ref pairs) => {
                if pairs.is_empty() {
                    Err(Error::Misc("empty query"))
                } else {
                    // this code assumes that the first key is always the command
                    let cmd = pairs[0].0.as_str();
                    // TODO let cmd = cmd.ToLower();
                    let res =
                        match cmd {
                            //"explain" => reply_explain clientMsg db
                            //"aggregate" => reply_aggregate clientMsg db
                            "insert" => self.reply_Insert(clientMsg, db),
                            //"delete" => reply_Delete clientMsg db
                            //"distinct" => reply_distinct clientMsg db
                            //"update" => reply_Update clientMsg db
                            //"findandmodify" => reply_FindAndModify clientMsg db
                            //"count" => reply_Count clientMsg db
                            //"validate" => reply_Validate clientMsg db
                            //"createindexes" => reply_createIndexes clientMsg db
                            //"deleteindexes" => reply_deleteIndexes clientMsg db
                            //"drop" => reply_DropCollection clientMsg db
                            //"dropdatabase" => reply_DropDatabase clientMsg db
                            //"listcollections" => reply_listCollections clientMsg db
                            //"listindexes" => reply_listIndexes clientMsg db
                            //"create" => reply_CreateCollection clientMsg db
                            //"features" => reply_features clientMsg db
                            _ => Err(Error::Misc("unknown cmd"))
                        };
                    res
                }
            },
            _ => Err(Error::Misc("query must be a document")),
        }
    }

    fn reply2004(&self, qm: BsonMsgQuery) -> BsonMsgReply {
        let pair = qm.q_fullCollectionName.split('.').collect::<Vec<_>>();
        let r = 
            if pair.len() < 2 {
                // TODO failwith (sprintf "bad collection name: %s" (clientMsg.q_fullCollectionName))
                Err(Error::Misc("bad collection name"))
            } else {
                let db = pair[0];
                let coll = pair[1];
                if db == "admin" {
                    if coll == "$cmd" {
                        //reply_AdminCmd clientMsg
                        Err(Error::Misc("TODO"))
                    } else {
                        //failwith "not implemented"
                        Err(Error::Misc("TODO"))
                    }
                } else {
                    if coll == "$cmd" {
                        if pair.len() == 4 && pair[2]=="sys" && pair[3]=="inprog" {
                            //reply_cmd_sys_inprog clientMsg db
                        Err(Error::Misc("TODO"))
                        } else {
                            self.reply_cmd(&qm, db)
                        }
                    } else if pair.len()==3 && pair[1]=="system" && pair[2]=="indexes" {
                        //reply_system_indexes clientMsg db
                        Err(Error::Misc("TODO"))
                    } else if pair.len()==3 && pair[1]=="system" && pair[2]=="namespaces" {
                        //reply_system_namespaces clientMsg db
                        Err(Error::Misc("TODO"))
                    } else {
                        //reply_Query clientMsg
                        Err(Error::Misc("TODO"))
                    }
                }
            };
        match r {
            Ok(r) => r,
            Err(e) => reply_err(qm.q_requestID, e),
        }
    }

    fn handle_one_message(&self, stream: &mut std::net::TcpStream) -> Result<()> {
        let ba = try!(readMessage(stream));
        let msg = try!(parseMessageFromClient(&ba));
        match msg {
            BsonClientMsg::BsonMsgKillCursors(km) => {
            },
            BsonClientMsg::BsonMsgQuery(qm) => {
                let resp = self.reply2004(qm);
                let ba = resp.encodeReply();
                stream.write(&ba);
            },
            BsonClientMsg::BsonMsgGetMore(gm) => {
            },
        }
        Ok(())
    }

    fn handle_client(&self, mut stream: std::net::TcpStream) -> Result<()> {
        loop {
            let r = self.handle_one_message(&mut stream);
            if r.is_err() {
                // TODO
                return r;
            }
        }
    }

}

pub fn serve() {
    let listener = std::net::TcpListener::bind("127.0.0.1:80").unwrap();

    // accept connections and process them, spawning a new thread for each one
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                std::thread::spawn(move|| {
                    // connection succeeded
                    let conn = elmo_sqlite3::connect("foo.db").expect("TODO");
                    let conn = elmo::Connection::new(conn);
                    let s = Server {
                        conn: conn,
                    };
                    s.handle_client(stream).expect("TODO");
                });
            }
            Err(e) => { /* connection failed */ }
        }
    }

    // close the socket server
    drop(listener);
}


