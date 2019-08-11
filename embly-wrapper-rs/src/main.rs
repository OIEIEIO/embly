use bimap::BidirectionalMap;
use error::Result;
use lucet_wasi;
use protobuf::parse_from_bytes;
use protobuf::Message as _;
use protos::comms::Message;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::io::prelude::*;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::{cmp, env, thread, time};

use lucet_runtime::lucet_hostcalls;
use lucet_runtime::{DlModule, Limits, MmapRegion, Module, Region};
use lucet_runtime_internals::module::ModuleInternal;
use lucet_runtime_internals::val;
use lucet_wasi::{memory, wasm32, WasiCtxBuilder};
use std::ffi::OsStr;
use std::os::unix::prelude::OsStrExt;

mod bimap;
mod error;
mod protos;

fn as_u32_le(array: &[u8]) -> u32 {
    u32::from(array[0])
        | (u32::from(array[1]) << 8)
        | (u32::from(array[2]) << 16)
        | (u32::from(array[3]) << 24)
}
fn u32_as_u8_le(x: u32) -> [u8; 4] {
    [
        (x & 0xff) as u8,
        ((x >> 8) & 0xff) as u8,
        ((x >> 16) & 0xff) as u8,
        ((x >> 24) & 0xff) as u8,
    ]
}

fn u64_as_u8_le(x: u64) -> [u8; 8] {
    [
        (x & 0xff) as u8,
        ((x >> 8) & 0xff) as u8,
        ((x >> 16) & 0xff) as u8,
        ((x >> 24) & 0xff) as u8,
        ((x >> 32) & 0xff) as u8,
        ((x >> 40) & 0xff) as u8,
        ((x >> 48) & 0xff) as u8,
        ((x >> 56) & 0xff) as u8,
    ]
}

lucet_hostcalls! {

    #[no_mangle] pub unsafe extern "C"
    fn __read(
        &mut vmctx,
        id: wasm32::uintptr_t,
        payload: wasm32::uintptr_t,
        payload_len: wasm32::uintptr_t,
        ln: wasm32::uintptr_t,
    ) -> wasm32::__wasi_errno_t {
        let mut ctx = vmctx.get_embed_ctx_mut::<EmblyCtx>();
        match memory::dec_slice_of_mut::<u8>(vmctx, payload as u32, payload_len as u32) {
            Ok(bytes) => {
                let read = ctx.read(id as i32, bytes).expect("should have been able to read");
                memory::enc_usize_byref(vmctx, ln, read).expect("should enc read byref");
            },
            Err(e) => panic!("TODO {}", e),
        };
        0
    }

    #[no_mangle] pub unsafe extern "C"
    fn __write(
        &mut vmctx,
        id: wasm32::uintptr_t,
        payload: wasm32::uintptr_t,
        payload_len: wasm32::uintptr_t,
        ln: wasm32::uintptr_t,
    ) -> wasm32::__wasi_errno_t {
        let mut ctx = vmctx.get_embed_ctx_mut::<EmblyCtx>();
        match memory::dec_slice_of::<u8>(vmctx, payload as u32, payload_len as u32) {
            Ok(bytes) => {
                let written = ctx.write(id as i32, bytes).expect("should have been able to write");
                memory::enc_usize_byref(vmctx, ln, written).unwrap();
            },
            Err(e) => panic!("TODO {}", e),
        };
        println!("__write {:?}", (ctx.address, ctx.address_count, id, payload, payload_len, ln));
        0
    }

    #[no_mangle] pub unsafe extern "C"
    fn __spawn(
        &mut vmctx,
        name: wasm32::uintptr_t,
        name_len: wasm32::uintptr_t,
        id: wasm32::uintptr_t,
    ) -> wasm32::__wasi_errno_t {
        let mut ctx = vmctx.get_embed_ctx_mut::<EmblyCtx>();
        let name = match memory::dec_slice_of::<u8>(vmctx, name as u32, name_len as u32) {
            Ok(bytes) => OsStr::from_bytes(bytes),
            Err(e) => panic!("TODO: {}", e),
        };
        let addr = ctx.spawn(name.to_str().unwrap()).expect("should be able to spawn");
        // TODO: problem that this is usize?
        memory::enc_usize_byref(vmctx, id, addr as usize).unwrap();
        0
    }

    #[no_mangle] pub unsafe extern "C"
    fn __events(
        &mut vmctx,
        non_blocking: wasm32::uint8_t,
        timeout_s: wasm32::uint64_t,
        timeout_ns: wasm32::uint32_t,
        ids: wasm32::uintptr_t,
        ids_len: wasm32::uint32_t,
        ln: wasm32::uintptr_t,
    ) -> wasm32::__wasi_errno_t {
        let mut ctx = vmctx.get_embed_ctx_mut::<EmblyCtx>();
        let timeout = if non_blocking != 0 {
            Some(time::Duration::new(timeout_s, timeout_ns))
        } else {
            None
        };
        let in_len = ids_len as usize;
        println!("is this working? {}", in_len);
        let mut events = ctx.events_limited(timeout, in_len).unwrap();
        println!("what events {:?}", events);
        events.resize(in_len, 0);
        memory::enc_usize_byref(vmctx, ln, events.len()).unwrap();
        memory::enc_slice_of(vmctx, &events, ids).unwrap();
        0
    }

}
struct EmblyCtx {
    receiver: Receiver<Message>,
    stream_writer: UnixStream,
    address_map: BidirectionalMap<i32, u64>,
    read_buffers: HashMap<i32, VecDeque<Message>>,
    address_count: i32,
    address: u64,
    pending_events: Vec<i32>,
}

impl EmblyCtx {
    fn new(
        receiver: Receiver<Message>,
        stream_writer: UnixStream,
        address: u64,
        master: u64,
    ) -> Self {
        let address_map = BidirectionalMap::new();
        let mut ctx = Self {
            receiver,
            stream_writer,
            address_map,
            address,
            address_count: 0,
            read_buffers: HashMap::new(),
            pending_events: Vec::new(),
        };
        ctx.add_address(master);
        ctx
    }

    fn write(&mut self, id: i32, buf: &[u8]) -> Result<usize> {
        let mut msg = Message::new();
        msg.set_to(
            *self
                .address_map
                .get_value(id)
                .ok_or(error::Error::DescriptorDoesntExist)?,
        );
        msg.set_from(self.address);
        msg.set_data(buf.to_vec());
        self.write_msg(msg)?;
        Ok(buf.len())
    }

    fn read(&mut self, id: i32, buf: &mut [u8]) -> Result<usize> {
        self.process_messages(Some(time::Duration::new(0, 0)))?;

        if let Some(queue) = self.read_buffers.get_mut(&id) {
            if queue.len() == 0 {
                return Ok(0);
            }
            let msg = queue.get_mut(0).expect("there should be something here");
            let msg_data_ln = msg.get_data().len();
            let to_drain = cmp::min(buf.len(), msg_data_ln);
            let part: Vec<u8> = msg.mut_data().drain(..to_drain).collect();
            buf[..to_drain].copy_from_slice(&part);
            if msg.get_data().len() == 0 {
                queue.pop_front();
            }
            Ok(part.len())
        } else {
            println!("no buffers for id");
            Ok(0)
        }
    }

    fn save_msg(&mut self, msg: Message) -> Result<i32> {
        if msg.from == 0 {
            print!("message has invalid from of 0 {:?}", msg)
            // TODO: err
        }
        if msg.to == 0 {
            print!("message has invalid to of 0 {:?}", msg)
            // TODO: err
        }

        let addr = self.add_address(msg.from);
        println!("save_msg_addr {:?}", (addr, msg.from));
        if self.read_buffers.get(&addr).is_none() {
            self.read_buffers.insert(addr, VecDeque::new());
        }
        let buf = self.read_buffers.get_mut(&addr).unwrap();
        buf.push_back(msg);
        Ok(addr)
    }

    fn process_messages(&mut self, timeout: Option<time::Duration>) -> Result<()> {
        let mut new: Vec<Message> = self.receiver.try_iter().collect();

        // if we have events we return
        if new.len() == 0 {
            if let Some(dur) = timeout {
                if let Ok(msg) = self.receiver.recv_timeout(dur) {
                    new.push(msg) // block forever
                } // block forever
            } else {
                // block forever
                // if no timeout is given we block fore                // block foreverver
                if let Ok(msg) = self.receiver.recv() {
                    new.push(msg)
                }
            }
        }
        for msg in new.drain(..) {
            let i = self.save_msg(msg)?;
            self.pending_events.push(i);
        }
        Ok(())
    }

    fn events_limited(
        &mut self,
        timeout: Option<time::Duration>,
        limit: usize,
    ) -> Result<Vec<i32>> {
        self.process_messages(timeout)?;
        let to_drain = cmp::min(self.pending_events.len(), limit);
        Ok(self.pending_events.drain(..to_drain).collect())
    }

    fn events(&mut self, timeout: Option<time::Duration>) -> Result<Vec<i32>> {
        self.process_messages(timeout)?;
        Ok(self.pending_events.drain(..).collect())
    }

    fn add_address(&mut self, addr: u64) -> i32 {
        if let Some(k) = self.address_map.get_key(addr) {
            return *k;
        }
        self.address_count += 1;
        self.address_map.insert(self.address_count, addr);
        self.address_count
    }

    fn spawn(&mut self, name: &str) -> Result<i32> {
        let mut msg = Message::new();
        msg.set_spawn(name.to_string());
        msg.set_to(1);
        msg.set_from(self.address);

        let spawn_addr = rand::random::<u64>();
        let addr = self.add_address(spawn_addr);
        msg.set_spawn_address(spawn_addr);

        // TODO! for now we generate the address ourselves here. This is just the easiest
        // because the function immediately knows where to send bytes to and the master
        // will receive events in order and be able to sort it out. Alternatively this
        // function would need be issues addresses to allocate or wait for a response

        self.write_msg(msg)?;
        Ok(addr)
    }

    fn exit(&mut self, code: i32) -> Result<()> {
        let mut msg = Message::new();
        msg.set_to(1);
        msg.set_from(1);
        msg.set_exit(code);
        self.write_msg(msg)
    }
    fn write_msg(&mut self, msg: Message) -> Result<()> {
        write_msg(&mut self.stream_writer, msg)
    }
}

fn write_msg(stream: &mut UnixStream, msg: Message) -> Result<()> {
    let msg_bytes = msg.write_to_bytes()?;
    stream.write(&u32_as_u8_le(msg_bytes.len() as u32))?;
    stream.write(&msg_bytes)?;
    Ok(())
}

fn next_message(stream: &mut UnixStream) -> Result<Message> {
    let mut size_bytes: [u8; 4] = [0; 4];
    stream.read(&mut size_bytes)?;
    let size = as_u32_le(&size_bytes) as usize;
    let mut read = 0;
    let mut msg_bytes = vec![0; size];
    loop {
        let ln = stream.read(&mut msg_bytes[read..])?;
        read += ln;
        println!(
            "reading msg {:?}",
            (ln, msg_bytes[read..].len(), read, size)
        );
        if ln == 0 || read == size {
            break;
        }
    }
    let msg: Message = parse_from_bytes(&msg_bytes)?;
    Ok(msg)
}

fn main() -> Result<()> {
    lucet_wasi::hostcalls::ensure_linked();
    lucet_runtime::lucet_internal_ensure_linked();

    let addr_string =
        env::var("EMBLY_ADDR").expect("EMBLY_ADDR environment variable should be available");
    let embly_module =
        env::var("EMBLY_MODULE").expect("EMBLY_MODULE environment variable should be available");

    let module = DlModule::load(&embly_module)?;

    let min_globals_size = module.globals().len() * std::mem::size_of::<u64>();
    let globals_size = ((min_globals_size + 4096 - 1) / 4096) * 4096;
    let region = MmapRegion::create(
        1,
        &Limits {
            globals_size,
            heap_memory_size: 4294967296,
            stack_size: 8388608,
            heap_address_space_size: 8589934592,
        },
    )?;
    let ctx = WasiCtxBuilder::new().inherit_stdio();

    let mut stream_reader = UnixStream::connect("/tmp/embly.sock")?;
    let stream_writer = stream_reader.try_clone()?;
    let mut stream_closer = stream_reader.try_clone()?;

    let (sender, receiver) = channel();

    let addr = addr_string.parse::<u64>().unwrap();
    stream_reader.write(&u64_as_u8_le(addr))?;
    thread::spawn(move || loop {
        println!("reading bytes");
        let msg = next_message(&mut stream_reader).unwrap();
        // channel has an infinite buffer, so this is where our messages go
        sender.send(msg).unwrap();
    });

    let msg: Message = receiver.recv()?;
    println!("got first message {:?}", msg);
    if msg.parent_address == 0 || msg.your_address == 0 {
        return Err(error::Error::InvalidStartup(msg));
    }

    if msg.your_address != addr {
        panic!("addr doesn't match {} {}", addr, msg.your_address)
    }

    let embly_ctx = EmblyCtx::new(
        receiver,
        stream_writer,
        msg.your_address,
        msg.parent_address,
    );

    let mut inst = region
        .new_instance_builder(module as Arc<dyn Module>)
        .with_embed_ctx(ctx.build().expect("WASI ctx can be created"))
        .with_embed_ctx(embly_ctx)
        .build()?;

    let exit_code = match inst.run("main", &[val::Val::I32(0), val::Val::I32(0)]) {
        // normal termination implies 0 exit code
        Ok(_) => 0,
        Err(lucet_runtime::Error::RuntimeTerminated(
            lucet_runtime::TerminationDetails::Provided(any),
        )) => *any
            .downcast_ref::<lucet_wasi::host::__wasi_exitcode_t>()
            .expect("termination yields an exitcode"),
        Err(e) => panic!("lucet-wasi runtime error: {}", e),
    };

    let mut msg = Message::new();
    msg.exit = exit_code as i32; //todo: u32
    write_msg(&mut stream_closer, msg)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use std::str;
    use std::sync::mpsc;
    use std::time;

    const FUNC_ADDRESS: u64 = 8700;
    const MASTER: u64 = 8701;

    fn new_ctx() -> (EmblyCtx, mpsc::Sender<Message>, UnixStream) {
        let (sock1, sock2) = UnixStream::pair().unwrap();
        let (sender, receiver) = channel();
        let ctx = EmblyCtx::new(receiver, sock1, FUNC_ADDRESS, MASTER);
        (ctx, sender, sock2)
    }

    fn assert_send_and_read(
        id: i32,
        from: u64,
        to: u64,
        ctx: &mut EmblyCtx,
        sender: mpsc::Sender<Message>,
    ) {
        let mut msg = Message::new();
        msg.set_data(b"hello".to_vec());
        msg.set_from(from);
        msg.set_to(to);
        sender.send(msg).unwrap();

        let events = ctx.events(Some(time::Duration::new(0, 0))).unwrap();
        println!("{:?}", events);
        assert_eq!(1, events.len());
        let mut buf = vec![0; 4096];
        let ln = ctx.read(id, &mut buf).unwrap() as usize;
        println!("{}", ln);
        assert_eq!(str::from_utf8(&buf[..ln]).unwrap(), "hello");
    }

    #[test]
    fn test_basic_read() {
        let (mut ctx, sender, _stream) = new_ctx();

        assert_eq!(
            0,
            ctx.events(Some(time::Duration::new(0, 0))).unwrap().len()
        );

        assert_send_and_read(1, MASTER, FUNC_ADDRESS, &mut ctx, sender);
    }
    #[test]
    fn test_spawn() {
        let (mut ctx, sender, mut stream) = new_ctx();

        let addr = ctx.spawn("name").unwrap();

        let msg = next_message(&mut stream).unwrap();
        assert_eq!(msg.spawn, "name");
        let spawn_addr = msg.spawn_address;
        assert_eq!(msg.spawn_address, *ctx.address_map.get_value(addr).unwrap());

        assert_send_and_read(addr, spawn_addr, FUNC_ADDRESS, &mut ctx, sender);
    }
}