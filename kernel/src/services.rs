use crate::arch;
use crate::arch::mem::MemoryMapping;
use crate::arch::process::ProcessHandle;
pub use crate::arch::ProcessContext;
use crate::args::KernelArguments;
use crate::filled_array;
use crate::mem::{MemoryManagerHandle, PAGE_SIZE};
use crate::server::Server;
use core::{mem, slice};
use xous::{ MemoryFlags, CID, PID, SID};

const MAX_PROCESS_COUNT: usize = 32;
const MAX_SERVER_COUNT: usize = 32;
const DEFAULT_STACK_SIZE: usize = 131072;
pub use crate::arch::mem::DEFAULT_STACK_TOP;

/// This is the address a program will jump to in order
/// to return from an ISR.
pub const RETURN_FROM_ISR: usize = 0xff80_2000;

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum ProcessState {
    /// This is an unallocated, free process
    Free,

    /// This is a brand-new process that hasn't been run
    /// yet, and needs its stack and entrypoint set up.
    Setup(
        usize, /* entrypoint */
        usize, /* stack */
        usize, /* stack size */
    ),

    /// This process is able to be run
    Ready,

    /// This is the current active process
    Running,

    /// This process is waiting for a

    /// This process is waiting for an event, such as
    /// as message or an interrupt
    Sleeping,
}

impl Default for ProcessState {
    fn default() -> ProcessState {
        ProcessState::Free
    }
}

#[derive(Copy, Clone, Default)]
pub struct Process {
    /// The absolute MMU address.  If 0, then this process is free.  This needs
    /// to be available so we can switch to this process at any time, so it
    /// cannot go into the "inner" struct.
    pub mapping: MemoryMapping,

    /// Where this process is in terms of lifecycle
    state: ProcessState,

    /// The process that created this process, which tells
    /// who is allowed to manipulate this process.
    pub ppid: PID,
}

/// This is per-process data.  The arch-specific definitions will instantiate
/// this struct in order to avoid the need to statically-allocate this for
/// all possible processes.
/// Note that this data is only available when the current process is active.
#[repr(C)]
#[derive(Debug)]
pub struct ProcessInner {
    /// Default virtual address when MapMemory is called with no `virt`
    pub mem_default_base: usize,

    /// The last address allocated from
    pub mem_default_last: usize,

    /// Address where messages are passed into
    pub mem_message_base: usize,

    /// The last address that was allocated from
    pub mem_message_last: usize,

    /// Base address of the heap
    pub mem_heap_base: usize,

    /// Current size of the heap
    pub mem_heap_size: usize,

    /// Maximum size of the heap
    pub mem_heap_max: usize,

    /// A mapping of connection IDs to servers
    pub connection_map: [u8; 32],
    pub _reserved: [u8; 28],
}

impl Default for ProcessInner {
    fn default() -> Self {
        ProcessInner {
            mem_default_base: arch::mem::DEFAULT_BASE,
            mem_default_last: arch::mem::DEFAULT_BASE,
            mem_message_base: arch::mem::DEFAULT_MESSAGE_BASE,
            mem_message_last: arch::mem::DEFAULT_MESSAGE_BASE,
            mem_heap_base: arch::mem::DEFAULT_HEAP_BASE,
            mem_heap_size: 0,
            mem_heap_max: 524288,
            connection_map: [0; 32],
            _reserved: [0; 28],
        }
    }
}

impl Process {
    pub fn runnable(&self) -> bool {
        match self.state {
            ProcessState::Setup(_, _, _) | ProcessState::Ready => true,
            _ => false,
        }
    }

    pub fn current_context_nr() -> usize {
        ProcessHandle::get().current_context_nr()
    }
}

#[repr(C)]
/// The stage1 bootloader sets up some initial processes.  These are reported
/// to us as (satp, entrypoint, sp) tuples, which can be turned into a structure.
/// The first element is always the kernel.
pub struct InitialProcess {
    /// The RISC-V SATP value, which includes the offset of the root page
    /// table plus the process ID.
    satp: usize,

    /// Where execution begins
    entrypoint: usize,

    /// Address of the top of the stack
    sp: usize,
}

/// A big unifying struct containing all of the system state.
/// This is inherited from the stage 1 bootloader.
pub struct SystemServices {
    /// Current PID
    pid: PID,

    /// A table of all processes in the system
    pub processes: [Process; MAX_PROCESS_COUNT],

    /// A table of all servers in the system
    servers: [Option<Server>; MAX_SERVER_COUNT],

    /// A log of the currently-active syscall depth
    syscall_stack: [(usize, usize); 3],

    /// How many entries there are on the syscall stack
    syscall_depth: usize,
}

static mut SYSTEM_SERVICES: SystemServices = SystemServices {
    pid: 1 as PID,
    processes: [Process {
        state: ProcessState::Free,
        ppid: 0,
        mapping: arch::mem::DEFAULT_MEMORY_MAPPING,
    }; MAX_PROCESS_COUNT],
    servers: filled_array![None; 32], // Note we can't use MAX_SERVER_COUNT here because of how Rust's tokenization works
    syscall_stack: [(0, 0), (0, 0), (0, 0)],
    syscall_depth: 0,
};

impl core::fmt::Debug for Process {
    fn fmt(&self, fmt: &mut core::fmt::Formatter) -> core::result::Result<(), core::fmt::Error> {
        write!(
            fmt,
            "Process state: {:?}  Memory mapping: {:?}",
            self.state, self.mapping
        )
    }
}

impl SystemServices {
    /// Create a new "System Services" object based on the arguments from the kernel.
    /// These arguments decide where the memory spaces are located, as well as where
    /// the stack and program counter should initially go.
    pub fn init(&mut self, base: *const u32, args: &KernelArguments) {
        // Look through the kernel arguments and create a new process for each.
        let init_offsets = {
            let mut init_count = 1;
            for arg in args.iter() {
                if arg.name == make_type!("IniE") {
                    init_count += 1;
                }
            }
            unsafe { slice::from_raw_parts(base as *const InitialProcess, init_count) }
        };

        // Copy over the initial process list.  The pid is encoded in the SATP value
        // from the bootloader.  For each process, translate it from a raw KernelArguments
        // value to a SystemServices Process value.
        for init in init_offsets.iter() {
            let pid = (init.satp >> 22) & ((1 << 9) - 1);
            let ref mut process = self.processes[(pid - 1) as usize];
            // println!(
            //     "Process: SATP: {:08x}  PID: {}  Memory: {:08x}  PC: {:08x}  SP: {:08x}  Index: {}",
            //     init.satp,
            //     pid,
            //     init.satp << 10,
            //     init.entrypoint,
            //     init.sp,
            //     pid - 1
            // );
            unsafe { process.mapping.from_raw(init.satp) };
            process.ppid = if pid == 1 { 0 } else { 1 };
            process.state = ProcessState::Setup(init.entrypoint, init.sp, DEFAULT_STACK_SIZE);
        }

        // Set up our handle with a bogus sp and pc.  These will get updated
        // once a context switch _away_ from the kernel occurs, however we need
        // to make sure other fields such as "thread number" are all valid.
        ProcessHandle::get().init(0, 0);
    }

    pub fn get_process(&self, pid: PID) -> Result<&Process, xous::Error> {
        if pid == 0 {
            println!("Process not found -- PID is 0");
            return Err(xous::Error::ProcessNotFound);
        }

        // PID0 doesn't exist -- process IDs are offset by 1.
        let pid_idx = pid as usize - 1;
        if self.processes[pid_idx].mapping.get_pid() != pid {
            println!(
                "Process doesn't match ({} vs {})",
                self.processes[pid_idx].mapping.get_pid(),
                pid
            );
            return Err(xous::Error::ProcessNotFound);
        }
        Ok(&self.processes[pid_idx])
    }

    pub fn get_process_mut(&mut self, pid: PID) -> Result<&mut Process, xous::Error> {
        if pid == 0 {
            println!("Process not found -- PID is 0");
            return Err(xous::Error::ProcessNotFound);
        }

        // PID0 doesn't exist -- process IDs are offset by 1.
        let pid_idx = pid as usize - 1;
        if self.processes[pid_idx].mapping.get_pid() != pid {
            println!(
                "Process doesn't match ({} vs {})",
                self.processes[pid_idx].mapping.get_pid(),
                pid
            );
            return Err(xous::Error::ProcessNotFound);
        }
        Ok(&mut self.processes[pid_idx])
    }

    pub fn current_pid(&self) -> PID {
        let pid = arch::current_pid();
        assert_ne!(pid, 0, "no current process");
        // PID0 doesn't exist -- process IDs are offset by 1.
        assert_eq!(
            self.processes[pid as usize - 1].mapping,
            MemoryMapping::current(),
            "process memory map doesn't match -- current_pid: {}",
            pid
        );
        assert_eq!(
            pid, self.pid,
            "current pid {} doesn't match arch pid: {}",
            self.pid, pid
        );
        pid as PID
    }

    /// Create a stack frame in the specified process and jump to it.
    /// 1. Pause the current process and switch to the new one
    /// 2. Save the process state, if it hasn't already been saved
    /// 3. Run the new process, returning to an illegal instruction
    pub fn make_callback_to(
        &mut self,
        pid: PID,
        pc: *const usize,
        irq_no: usize,
        arg: *mut usize,
    ) -> Result<(), xous::Error> {
        // Get the current process (which was just interrupted) and mark
        // it as "ready to run".
        {
            let current_pid = self.current_pid();
            let mut current = self
                .get_process_mut(current_pid)
                .expect("couldn't get current PID");
            assert_eq!(
                current.state,
                ProcessState::Running,
                "current process was not running"
            );
            current.state = ProcessState::Ready;
        }

        // Get the new process, and ensure that it is in a state where it's fit to run.
        let mut process = self.get_process_mut(pid)?;
        match process.state {
            ProcessState::Ready | ProcessState::Running | ProcessState::Sleeping => (),
            ProcessState::Free => panic!("process was not allocated"),
            ProcessState::Setup(_, _, _) => panic!("process hasn't been set up yet"),
        }
        process.state = ProcessState::Running;

        // Switch to new process memory space, allowing us to save the context
        // if necessary.
        process.mapping.activate();
        self.pid = pid;

        let mut process = ProcessHandle::get();
        let sp = process.current_context().stack_pointer();
        process.bank();
        arch::syscall::invoke(
            process.trap_context(),
            pid == 1,
            pc as usize,
            sp,
            RETURN_FROM_ISR,
            &[irq_no, arg as usize],
        );
        Ok(())
    }

    /// Resume the given process, picking up exactly where it left off.
    /// If the process is in the Setup state, set it up and then resume.
    pub fn resume_pid(
        &mut self,
        pid: PID,
        previous_state: ProcessState,
    ) -> Result<(), xous::Error> {
        let previous_pid = self.current_pid();

        // Save state if the PID has changed
        if pid != previous_pid {
            self.pid = pid;
            let new = self.get_process_mut(pid)?;
            match new.state {
                ProcessState::Free => return Err(xous::Error::ProcessNotFound),
                _ => (),
            }

            // Perform the actual switch to the new memory space
            new.mapping.activate();

            // Set up the new process, if necessary
            match new.state {
                ProcessState::Setup(entrypoint, stack, stack_size) => {
                    let mut process = ProcessHandle::get();
                    println!(
                        "Initializing new process with stack size of {} bytes",
                        stack_size
                    );
                    process.init(entrypoint, stack);
                    // Mark the stack as "unallocated-but-free"
                    let init_sp = stack & !0xfff;
                    let mut memory_manager = MemoryManagerHandle::get();
                    memory_manager
                        .reserve_range(
                            (init_sp - stack_size) as *mut usize,
                            stack_size + 4096,
                            MemoryFlags::R | MemoryFlags::W,
                        )
                        .expect("couldn't reserve stack");
                }
                ProcessState::Free => panic!("process was suddenly Free"),
                ProcessState::Ready | ProcessState::Sleeping => (),
                ProcessState::Running => panic!("process was already running"),
            }
            new.state = ProcessState::Running;

            // Mark the previous process as ready to run, since we just switched away
            {
                // println!(
                //     "Marking previous process {} as {:?}",
                //     previous_pid, previous_state
                // );
                self.get_process_mut(previous_pid)
                    .expect("couldn't get previous pid")
                    .state = previous_state;
            }
        }

        let mut process = ProcessHandle::get();

        // Restore the previous context, if one exists.
        if process.trap_context().valid() {
            process.trap_context().invalidate();
        }

        Ok(())
    }

    /// Allocate a new server ID for this process and return the address.
    /// If the server table is full, return an error.
    pub fn create_server(&mut self, name: usize) -> Result<SID, xous::Error> {
        println!("Looking through server list for free server");
        println!("Server entries are {} bytes long", mem::size_of::<Server>());

        for entry in self.servers.iter_mut() {
            if entry == &None {
                println!("Found a free slot.  Allocating an entry");
                let pid = self.pid;
                let sid = (pid as usize, name as usize, pid as usize, name as usize);
                let (addr, size) = {
                    let mut mm = MemoryManagerHandle::get();
                    (mm.map_zeroed_page(pid, false)?, PAGE_SIZE)
                };
                Server::init(entry, pid, sid, addr, size).or_else(|x| {
                    let mut mm = MemoryManagerHandle::get();
                    mm.unmap_page(addr);
                    Err(x)
                })?;
                return Ok(sid);
            }
        }
        Err(xous::Error::OutOfMemory)
    }

    /// Allocate a new server ID for this process and return the address.
    /// If the server table is full, return an error.
    pub fn connect_to_server(&mut self, sid: SID) -> Result<CID, xous::Error> {
        // Check to see if we've already connected to this server.
        // While doing this, find a free slot in case we haven't
        // yet connected.
        let mut slot_idx = None;
        let mut process = ProcessHandle::get();

        // Look through the connection map for (1) a free slot, and (2) an existing connection
        for (idx, server_idx) in process.inner.connection_map.iter().enumerate() {
            // If we find an empty slot, use it
            if *server_idx == 0 {
                slot_idx = Some(idx);
            }
            // If a connection to this server ID exists already, return it.
            if let Some(allocated_server) = &self.servers[*server_idx as usize] {
                if allocated_server.sid == sid {
                    return Ok(idx as CID + 1);
                }
            }
        }
        let slot_idx = slot_idx.ok_or_else(|| xous::Error::OutOfMemory)?;

        // Look through all servers for one whose SID matches.
        for (idx, server) in self.servers.iter().enumerate() {
            if let Some(allocated_server) = server {
                if allocated_server.sid == sid {
                    process.inner.connection_map[slot_idx] = idx as u8 + 1;
                    return Ok(idx + 1);
                }
            }
        }
        Err(xous::Error::OutOfMemory)
    }

    /// Return a server based on the connection id and the current process
    pub fn server_from_cid(&mut self, cid: CID) -> Option<&mut Server> {
        if cid == 0 {
            println!("CID is 0, returning");
            return None;
        }
        let cid = cid - 1;
        let process = ProcessHandle::get();
        if cid >= process.inner.connection_map.len() {
            println!("CID {} > connection map len", cid);
            return None;
        }
        let server_idx = process.inner.connection_map[cid] as usize;
        if server_idx >= self.servers.len() {
            println!("CID {} and server_idx >= {}", cid, server_idx);
            return None;
        }

        println!("Returning self.servers[{}]", server_idx);
        self.servers[server_idx].as_mut()
    }

    /// Get a server based on a SID
    pub fn server_mut(&mut self, sid: SID) -> Option<&mut Server> {
        for server in self.servers.iter_mut() {
            if let Some(active_server) = server {
                if active_server.sid == sid {
                    return server.as_mut();
                }
            }
        }
        None
    }
}

/// How many people have checked out the handle object.
/// This should be replaced by an AtomicUsize when we get
/// multicore support.
/// For now, we can get away with this since the memory manager
/// should only be accessed in an IRQ context.
static mut SS_HANDLE_COUNT: usize = 0;

pub struct SystemServicesHandle<'a> {
    manager: &'a mut SystemServices,
}

/// Wraps the MemoryManager in a safe mutex.  Because of this, accesses
/// to the Memory Manager should only be made during interrupt contexts.
impl<'a> SystemServicesHandle<'a> {
    /// Get the singleton memory manager.
    pub fn get() -> SystemServicesHandle<'a> {
        let count = unsafe {
            SS_HANDLE_COUNT += 1;
            SS_HANDLE_COUNT - 1
        };
        if count != 0 {
            panic!("Multiple users of SystemServicesHandle!");
        }
        SystemServicesHandle {
            manager: unsafe { &mut SYSTEM_SERVICES },
        }
    }
}

impl Drop for SystemServicesHandle<'_> {
    fn drop(&mut self) {
        unsafe { SS_HANDLE_COUNT -= 1 };
    }
}

use core::ops::{Deref, DerefMut};
impl Deref for SystemServicesHandle<'_> {
    type Target = SystemServices;
    fn deref(&self) -> &SystemServices {
        &*self.manager
    }
}
impl DerefMut for SystemServicesHandle<'_> {
    fn deref_mut(&mut self) -> &mut SystemServices {
        &mut *self.manager
    }
}
