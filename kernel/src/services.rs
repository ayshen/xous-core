use crate::arch;
use crate::arch::mem::MemoryMapping;
use crate::arch::process::ProcessHandle;
pub use crate::arch::ProcessContext;
use crate::args::KernelArguments;
use crate::filled_array;
use crate::mem::{MemoryManagerHandle, PAGE_SIZE};
use crate::server::Server;
use core::{mem, slice};
use xous::{CtxID, MemoryFlags, MemoryType, MessageEnvelope, CID, PID, SID};

const MAX_PROCESS_COUNT: usize = 32;
const MAX_SERVER_COUNT: usize = 32;
const DEFAULT_STACK_SIZE: usize = 131072;
// pub use crate::arch::mem::DEFAULT_STACK_TOP;

/// This is the address a program will jump to in order to return from an ISR.
pub const RETURN_FROM_ISR: usize = 0xff80_2000;

/// This is the address a thread will return to when it exits.
pub const EXIT_THREAD: usize = 0xff80_3000;

pub const INITIAL_CONTEXT: usize = 2;
pub const IRQ_CONTEXT: usize = 1;

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum ProcessState {
    /// This is an unallocated, free process
    Free,

    /// This is a brand-new process that hasn't been run yet, and needs its
    /// stack and entrypoint set up.
    Setup(
        usize, /* entrypoint */
        usize, /* stack */
        usize, /* stack size */
    ),

    /// This process is able to be run.  The context bitmask describes contexts
    /// that are ready.
    Ready(usize /* context bitmask */),

    /// This is the current active process.  The context bitmask describes
    /// contexts that are ready, excluding the currently-executing context.
    Running(usize /* context bitmask */),

    /// This process is waiting for an event, such as as message or an
    /// interrupt.  There are no contexts that can be run.
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

    /// The process that created this process, which tells who is allowed to
    /// manipulate this process.
    pub ppid: PID,

    /// The current context (i.e. thread)
    current_context: u8,

    /// The context number that was active before this process was switched
    /// away.
    previous_context: u8,
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

    /// A mapping of connection IDs to server indexes
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
            ProcessState::Setup(_, _, _) | ProcessState::Ready(_) => true,
            _ => false,
        }
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
    _syscall_stack: [(usize, usize); 3],

    /// How many entries there are on the syscall stack
    _syscall_depth: usize,
}

static mut SYSTEM_SERVICES: SystemServices = SystemServices {
    pid: 1 as PID,
    processes: [Process {
        state: ProcessState::Free,
        ppid: 0,
        mapping: arch::mem::DEFAULT_MEMORY_MAPPING,
        current_context: 0,
        previous_context: INITIAL_CONTEXT as u8,
    }; MAX_PROCESS_COUNT],
    // Note we can't use MAX_SERVER_COUNT here because of how Rust's
    // macro tokenization works
    servers: filled_array![None; 32],
    _syscall_stack: [(0, 0), (0, 0), (0, 0)],
    _syscall_depth: 0,
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
    /// Create a new "System Services" object based on the arguments from the
    /// kernel. These arguments decide where the memory spaces are located, as
    /// well as where the stack and program counter should initially go.
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

        // Copy over the initial process list.  The pid is encoded in the SATP
        // value from the bootloader.  For each process, translate it from a raw
        // KernelArguments value to a SystemServices Process value.
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
            if pid == 1 {
                process.ppid = 0;
                process.state = ProcessState::Running(0);
            } else {
                process.ppid = 1;
                process.state = ProcessState::Setup(init.entrypoint, init.sp, DEFAULT_STACK_SIZE);
            }
        }

        // Set up our handle with a bogus sp and pc.  These will get updated
        // once a context switch _away_ from the kernel occurs, however we need
        // to make sure other fields such as "thread number" are all valid.
        ProcessHandle::get().init(0, 0, INITIAL_CONTEXT);
        self.processes[0].current_context = INITIAL_CONTEXT as u8;
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

    pub fn current_context_nr(&self) -> usize {
        self.processes[self.pid as usize - 1].current_context as usize
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
    pub fn finish_callback_and_resume(
        &mut self,
        pid: PID,
        context: CtxID,
    ) -> Result<(), xous::Error> {
        // Get the current process (which was the interrupt handler) and mark it
        // as Ready.  Note that the new PID may very well be the same PID.
        {
            let current_pid = self.current_pid();
            let mut current = self
                .get_process_mut(current_pid)
                .expect("couldn't get current PID");
            current.state = match current.state {
                ProcessState::Running(0) => ProcessState::Sleeping,
                ProcessState::Running(x) => ProcessState::Ready(x),
                y => panic!("current process was {:?}, not 'Running(_)'", y),
            };
            current.current_context = current.previous_context;
        }

        // Get the new process, and ensure that it is in a state where it's fit
        // to run.  Again, if the new process isn't fit to run, then the system
        // is in a very bad state.
        {
            let mut process = self.get_process_mut(pid)?;
            // Ensure the new context is available to be run
            let available_contexts = match process.state {
                ProcessState::Ready(x) if x & 1 << context != 0 => x & !(1 << context),
                other => panic!(
                    "process was in an invalid state {:?} -- ctxid {} not available to run",
                    other, context
                ),
            };
            process.state = ProcessState::Running(available_contexts);
            process.current_context = context as u8;
            process.mapping.activate();

            // Activate the current context
            let mut arch_process = ProcessHandle::get();
            arch_process.set_context_nr(context);
        }
        self.pid = pid;
        Ok(())
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
        // Get the current process (which was just interrupted) and mark it as
        // "ready to run".  If this function is called when the current process
        // isn't running, that means the system has gotten into an invalid
        // state.
        {
            let current_pid = self.current_pid();
            let mut current = self
                .get_process_mut(current_pid)
                .expect("couldn't get current PID");
            current.state = match current.state {
                ProcessState::Running(x) => ProcessState::Ready(x | (1 << current.current_context)),
                y => panic!("current process was {:?}, not 'Running(_)'", y),
            };
            println!("Making PID {} state {:?}", current_pid, current.state);
        }

        // Get the new process, and ensure that it is in a state where it's fit
        // to run.  Again, if the new process isn't fit to run, then the system
        // is in a very bad state.
        {
            let mut process = self.get_process_mut(pid)?;
            let available_threads = match process.state {
                ProcessState::Ready(x) | ProcessState::Running(x) => x,
                ProcessState::Sleeping => 0,
                ProcessState::Free => panic!("process was not allocated"),
                ProcessState::Setup(_, _, _) => panic!("process hasn't been set up yet"),
            };
            process.state = ProcessState::Running(available_threads);
            process.previous_context = process.current_context;
            process.current_context = IRQ_CONTEXT as u8;
            process.mapping.activate();
        }

        // Switch to new process memory space, allowing us to save the context
        // if necessary.
        self.pid = pid;

        // Invoke the syscall, but use the current stack pointer.  When this
        // function returns, it will jump to the RETURN_FROM_ISR address,
        // causing an instruction fault and exiting the interrupt.
        let mut arch_process = ProcessHandle::get();
        let sp = arch_process.current_context().stack_pointer();

        // Activate the current context
        arch_process.set_context_nr(IRQ_CONTEXT);

        // Construct the new frame
        arch::syscall::invoke(
            arch_process.current_context(),
            pid == 1,
            pc as usize,
            sp,
            RETURN_FROM_ISR,
            &[irq_no, arg as usize],
        );
        Ok(())
    }

    /// Mark the specified context as ready to run
    pub fn ready_context(&mut self, pid: PID, context: CtxID) -> Result<(), xous::Error> {
        let process = self.get_process_mut(pid)?;
        process.state = match process.state {
            ProcessState::Running(x) if x & (1 << context) == 0 => {
                ProcessState::Running(x | (1 << context))
            }
            ProcessState::Ready(x) if x & (1 << context) == 0 => {
                ProcessState::Ready(x | (1 << context))
            }
            ProcessState::Sleeping => ProcessState::Ready(1 << context),
            other => panic!(
                "PID {} was not in a state to wake a context: {:?}",
                pid, other
            ),
        };
        Ok(())
    }

    pub fn set_context_result(
        &mut self,
        pid: PID,
        context: CtxID,
        result: xous::Result,
    ) -> Result<(), xous::Error> {
        let current_pid = self.current_pid();
        {
            let target_process = self.get_process(pid)?;
            target_process.mapping.activate();
            let mut arch_process = ProcessHandle::get();
            arch_process.set_context_result(context, result);
        }
        let current_process = self
            .get_process(current_pid)
            .expect("couldn't switch back after setting context result");
        current_process.mapping.activate();
        Ok(())
    }

    /// Resume the given process, picking up exactly where it left off. If the
    /// process is in the Setup state, set it up and then resume.
    pub fn activate_process_context(
        &mut self,
        new_pid: PID,
        mut new_context: CtxID,
        can_resume: bool,
        advance_context: bool,
    ) -> Result<CtxID, xous::Error> {
        let previous_pid = self.current_pid();
        let previous_context = self.current_context_nr();

        // Save state if the PID has changed.  This will activate the new memory
        // space.
        if new_pid != previous_pid {
            let new = self.get_process_mut(new_pid)?;

            // Ensure the new process can be run.
            match new.state {
                ProcessState::Free => return Err(xous::Error::ProcessNotFound),
                ProcessState::Setup(_, _, _) => new_context = INITIAL_CONTEXT,
                ProcessState::Running(x) | ProcessState::Ready(x) => {
                    // If no new context is specified, take the previous
                    // context.  If that is not runnable, do a round-robin
                    // search for the next available context.
                    assert!(
                        x != 0,
                        "process was {:?} but had no free contexts",
                        new.state
                    );
                    if new_context == 0 {
                        // print!(
                        //     "PID {}: Looking for a valid context in the mask {:08b}, curent context {} ({:08b})",
                        //     new_pid, x, new.current_context, new.current_context
                        // );
                        new_context = new.current_context as usize;
                        while x & (1 << new_context) == 0 {
                            new_context += 1;
                            if new_context > arch::process::MAX_CONTEXT {
                                new_context = 0;
                            }
                            // If we've looped around, return an error.
                            if new_context == new.current_context as usize {
                                println!("Looked through all contexts and couldn't find one that was ready");
                                return Err(xous::Error::ProcessNotFound);
                            }
                        }
                    // println!(" -- picked context {}", new_context);
                    } else if x & (1 << new_context) == 0 {
                        println!(
                            "context is {:?}, which is not valid for new context {}",
                            new.state, new_context
                        );
                        return Err(xous::Error::ProcessNotFound);
                    }
                }
                ProcessState::Sleeping => return Err(xous::Error::ProcessNotFound),
            }

            // Perform the actual switch to the new memory space.  From this
            // point onward, we will need to activate the previous memory space
            // if we encounter an error.
            new.mapping.activate();

            // Set up the new process, if necessary.  Remove the new context from
            // the list of ready contexts.
            new.state = match new.state {
                ProcessState::Setup(entrypoint, stack, stack_size) => {
                    let mut process = ProcessHandle::get();
                    println!(
                        "Initializing new process with stack size of {} bytes",
                        stack_size
                    );
                    process.init(entrypoint, stack, INITIAL_CONTEXT);
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
                    ProcessState::Running(0)
                }
                ProcessState::Free => panic!("process was suddenly Free"),
                ProcessState::Ready(x) | ProcessState::Running(x) => {
                    ProcessState::Running(x & !(1 << new_context))
                }
                ProcessState::Sleeping => ProcessState::Running(0),
            };

            // Mark the previous process as ready to run, since we just switched
            // away
            let previous = self
                .get_process_mut(previous_pid)
                .expect("couldn't get previous pid");
            previous.state = match previous.state {
                // If the previous process had exactly one thread that can be
                // run, then the Running thread list will be 0.  In that case,
                // we will either need to Sleep this process, or mark it as
                // being Ready to run.
                ProcessState::Running(x) if x == 0 => {
                    if can_resume {
                        ProcessState::Ready(1 << previous_context)
                    } else {
                        ProcessState::Sleeping
                    }
                }
                // Otherwise, there are additional threads that can be run.
                // Convert the previous process into "Ready", and include the
                // current context number only if `can_resume` is `true`.
                ProcessState::Running(x) => {
                    if can_resume {
                        ProcessState::Ready(x | (1 << previous_context))
                    } else {
                        ProcessState::Ready(x)
                    }
                }
                other => panic!(
                    "previous process PID {} was in an invalid state (not Running): {:?}",
                    previous_pid, other
                ),
            };
            if advance_context {
                previous.current_context += 1;
                if previous.current_context as CtxID > arch::process::MAX_CONTEXT {
                    previous.current_context = 0;
                }
            }
        // println!(
        //     "Set previous process PID {} state to {:?} (with can_resume = {})",
        //     previous_pid, previous.state, can_resume
        // );
        } else {
            if self.current_context_nr() == new_context {
                if !can_resume {
                    panic!("tried to switch to our own context without resume");
                }
                return Ok(new_context);
            }
            let new = self.get_process_mut(new_pid)?;
            new.state = match new.state {
                ProcessState::Running(x) if (x & 1 << new_context) == 0 => {
                    return Err(xous::Error::ProcessNotFound)
                }
                ProcessState::Running(x) => {
                    if can_resume {
                        ProcessState::Running((x | (1 << previous_context)) & !(1 << new_context))
                    } else {
                        ProcessState::Running(x | (1 << previous_context))
                    }
                }
                other => panic!(
                    "PID {} invalid process state (not Running): {:?}",
                    previous_pid, other
                ),
            };
            if advance_context {
                new.current_context += 1;
                if new.current_context as CtxID > arch::process::MAX_CONTEXT {
                    new.current_context = 0;
                }
            }
        }
        self.pid = new_pid;

        let mut process = ProcessHandle::get();

        // Restore the previous context, if one exists.
        process.set_context_nr(new_context);
        self.processes[self.pid as usize - 1].current_context = new_context as u8;
        let ctx = process.current_context();
        println!(
            "Switched to PID {}, context {}, with sepc: {:08x}",
            new_pid, new_context, ctx.sepc
        );

        Ok(new_context)
    }

    /// Move memory from one process to another.
    ///
    /// During this process, memory is deallocated from the first process, then
    /// we switch contexts and look for a free slot in the second process. After
    /// that, we switch back to the first process and return.
    ///
    /// If no free slot can be found, memory is re-attached to the first
    /// process.  By following this break-then-make approach, we avoid getting
    /// into a situation where memory may appear in two different processes at
    /// once.
    ///
    /// The given memory range is guaranteed to be unavailable in this process
    /// after this function returns.
    ///
    /// # Returns
    ///
    /// Returns the virtual address of the memory region in the target process.
    pub fn send_memory(
        &mut self,
        src_virt: *mut usize,
        dest_pid: PID,
        len: usize,
        writable: bool,
        _borrow: bool,
    ) -> Result<usize, xous::Error> {
        let current_pid = self.current_pid();
        let phys = {
            let mut error = None;
            let mut mm = MemoryManagerHandle::get();

            // Unmap each address from the current memory space.  If we
            // encounter an error, continue unmapping.
            let phys = mm.unmap_page(src_virt).unwrap_or_else(|e| {
                error = Some(e);
                0
            });
            for addr in
                ((src_virt as usize + PAGE_SIZE)..((src_virt as usize) + len)).step_by(PAGE_SIZE)
            {
                if let Err(e) = mm.unmap_page(addr as *mut usize) {
                    error = Some(e)
                }
            }
            if let Some(e) = error {
                return Err(e);
            }
            phys
        };

        // Switch to the target process, so we can manipulate its page tables.
        // From this point forward we can't use the `?` operator, since it would
        // leave us in the incorrect memory space.
        self.get_process(dest_pid)?.mapping.activate();

        let mut mm = MemoryManagerHandle::get();
        let mut flags = MemoryFlags::R;
        if writable {
            flags |= MemoryFlags::W;
        }
        let result = mm.map_range(
            phys as *mut usize,
            0 as *mut usize,
            len,
            dest_pid,
            flags,
            MemoryType::Messages,
        );
        if let Ok(ref range) = result {
            for offset in
                (range.addr.get()..(range.addr.get() + range.size.get())).step_by(PAGE_SIZE)
            {
                println!("Handing page to user");
                crate::arch::mem::hand_page_to_user(offset as *mut usize)
                    .expect("couldn't hand page to user");
            }
        }

        // Finally, switch back to the original process.
        self.get_process(current_pid)
            .expect("couldn't find previous process")
            .mapping
            .activate();
        println!(
            "send_memory: Sent phys {:08x} from {:08x} to {:08x}",
            phys,
            src_virt as usize,
            result.as_ref().unwrap().addr.get()
        );
        result.map(|virt| virt.addr.get())
    }

    pub fn spawn_thread(
        &mut self,
        entrypoint: *mut usize,
        stack_pointer: *mut usize,
        arg: *mut usize,
    ) -> Result<CtxID, xous::Error> {
        let mut process = ProcessHandle::get();
        let new_context_nr = process
            .find_free_context_nr()
            .ok_or(xous::Error::ContextNotAvailable)?;

        // Create the new context and set it to run in the new address space.
        let context = process.context(new_context_nr);
        arch::syscall::invoke(
            context,
            self.pid == 1,
            entrypoint as usize,
            stack_pointer as usize,
            EXIT_THREAD,
            &[arg as usize],
        );

        // Queue the thread to run
        let mut process = self
            .get_process_mut(self.current_pid())
            .expect("couldn't get current process");
        process.state = match process.state {
            ProcessState::Running(x) => ProcessState::Running(x | (1 << new_context_nr)),
            other => panic!(
                "error spawning thread: process was in an invalid state {:?}",
                other
            ),
        };

        Ok(new_context_nr)
    }

    /// Allocate a new server ID for this process and return the address. If the
    /// server table is full, return an error.
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
                    mm.unmap_page(addr)?;
                    Err(x)
                })?;
                return Ok(sid);
            }
        }
        Err(xous::Error::OutOfMemory)
    }

    /// Allocate a new server ID for this process and return the address. If the
    /// server table is full, return an error.
    pub fn connect_to_server(&mut self, sid: SID) -> Result<CID, xous::Error> {
        // Check to see if we've already connected to this server.
        // While doing this, find a free slot in case we haven't
        // yet connected.
        let mut slot_idx = None;
        let mut process = ProcessHandle::get();

        // Look through the connection map for (1) a free slot, and (2) an
        // existing connection
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
    pub fn server_from_sidx(&mut self, sidx: usize) -> Option<&mut Server> {
        if sidx > self.servers.len() {
            None
        } else {
            self.servers[sidx].as_mut()
        }
    }

    pub fn sidx_from_cid(&self, cid: CID) -> Option<usize> {
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
            None
        } else {
            Some(server_idx)
        }
    }

    pub fn queue_server_message(
        &mut self,
        sidx: usize,
        context: usize,
        envelope: MessageEnvelope,
    ) -> Result<(), xous::Error> {
        let current_pid = self.current_pid();
        let result = {
            let server_pid = self
                .server_from_sidx(sidx)
                .ok_or(xous::Error::ServerNotFound)?
                .pid;
            {
                let server_process = self.get_process(server_pid)?;
                server_process.mapping.activate();
            }
            let server = self
                .server_from_sidx(sidx)
                .expect("couldn't re-discover server index");
            server.queue_message(context, envelope)
        };
        let current_process = self
            .get_process(current_pid)
            .expect("couldn't restore previous process");
        current_process.mapping.activate();
        result
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

/// How many people have checked out the handle object. This should be replaced
/// by an AtomicUsize when we get multicore support. For now, we can get away
/// with this since the memory manager should only be accessed in an IRQ
/// context.
static mut SS_HANDLE_COUNT: usize = 0;

pub struct SystemServicesHandle<'a> {
    manager: &'a mut SystemServices,
}

/// Wraps the MemoryManager in a safe mutex.  Because of this, accesses to the
/// Memory Manager should only be made during interrupt contexts.
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
