//! Opt-in whole-tile media quantum. The pure gate has no IO; Sender adapts the
//! connection actor's monotonic clock and records actual flush observations.

pub const QUANTUM_BYTES: u32 = 16 * 1024;
pub const QUANTUM_DATAGRAMS: u32 = 16;
pub const SLOT_NS: u64 = 1_000_000;
pub const CURRENT_MAX_TILE_CHARGE: u32 = 4096 + 4 * (32 + 8 + 128);
pub const CURRENT_MAX_TILE_DATAGRAMS: u32 = 4;
const MAX_EXPLICIT_TILE_CHARGE: u32 = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub quantum_bytes: u32,
    pub quantum_datagrams: u32,
    pub slot_ns: u64,
    pub max_tile_bytes: u32,
    pub max_tile_datagrams: u32,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            quantum_bytes: QUANTUM_BYTES,
            quantum_datagrams: QUANTUM_DATAGRAMS,
            slot_ns: SLOT_NS,
            max_tile_bytes: CURRENT_MAX_TILE_CHARGE,
            max_tile_datagrams: CURRENT_MAX_TILE_DATAGRAMS,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TileCharge {
    bytes: u32,
    datagrams: u32,
}
impl TileCharge {
    pub fn bytes(self) -> u32 {
        self.bytes
    }
    pub fn datagrams(self) -> u32 {
        self.datagrams
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidConfiguration,
    InvalidTile,
    BackwardsClock,
    ClockOverflow,
    CounterOverflow,
    WrongGeneration,
    ActiveWake,
    StaleWake,
    StaleTicket,
    WrongBudgetWait,
}

impl std::fmt::Display for Error {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(output, "tile quantum: {self:?}")
    }
}
impl std::error::Error for Error {}

pub fn current_tile_charge(body: usize, stride: usize) -> Result<TileCharge, Error> {
    if !(1..=4096).contains(&body) || !(1024..=1168).contains(&stride) {
        return Err(Error::InvalidTile);
    }
    let fragments = body.div_ceil(stride);
    Ok(TileCharge {
        bytes: (body + fragments * (32 + 8 + 128)) as u32,
        datagrams: fragments as u32,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timer {
    generation: u64,
    serial: u64,
    due_ns: u64,
}
impl Timer {
    pub fn due_ns(self) -> u64 {
        self.due_ns
    }
    pub fn generation(self) -> u64 {
        self.generation
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WakeHandle {
    generation: u64,
    serial: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ticket {
    wake: WakeHandle,
    revision: u64,
    charge: TileCharge,
}
impl Ticket {
    pub fn charge(self) -> TileCharge {
        self.charge
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BudgetWait {
    wake: WakeHandle,
    revision: u64,
    charge: TileCharge,
    due_ns: u64,
}
impl BudgetWait {
    pub fn due_ns(self) -> u64 {
        self.due_ns
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    Ready(Ticket),
    BudgetBlocked(BudgetWait),
    QuinnBlocked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    Empty,
    QuinnBlocked,
    BudgetBlocked(BudgetWait),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WakeResult {
    pub handle: Option<WakeHandle>,
    pub encode_slot: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub generation: u64,
    pub credit_bytes: i64,
    pub credit_datagrams: i64,
    pub next_refill_ns: u64,
    pub timer: Option<Timer>,
    pub reserved_tiles: u64,
    pub charged_bytes: u64,
    pub charged_datagrams: u64,
}

#[derive(Debug)]
pub struct Quantum {
    quantum: u32,
    packet_quantum: u32,
    period: u64,
    max_tile: u32,
    max_tile_datagrams: u32,
    generation: u64,
    credit: i64,
    packet_credit: i64,
    last_now: u64,
    next_refill: u64,
    serial: u64,
    revision: u64,
    active: Option<WakeHandle>,
    timer: Option<Timer>,
    reserved_tiles: u64,
    charged_bytes: u64,
    charged_datagrams: u64,
}

impl Quantum {
    pub fn new(now_ns: u64, generation: u64) -> Result<Self, Error> {
        Self::with_config(now_ns, generation, Limits::default())
    }

    /// Explicit bounded component configurations, chiefly for tests/future
    /// quanta. Production glue must use new(); this adds no CLI or new tile cap.
    pub fn with_config(now_ns: u64, generation: u64, limits: Limits) -> Result<Self, Error> {
        let Limits {
            quantum_bytes: quantum,
            quantum_datagrams: packet_quantum,
            slot_ns: period,
            max_tile_bytes: max_tile,
            max_tile_datagrams,
        } = limits;
        if quantum == 0
            || quantum > MAX_EXPLICIT_TILE_CHARGE
            || period == 0
            || period > 1_000_000_000
            || max_tile == 0
            || max_tile > MAX_EXPLICIT_TILE_CHARGE
            || packet_quantum == 0
            || packet_quantum > 1024
            || max_tile_datagrams == 0
            || max_tile_datagrams > CURRENT_MAX_TILE_DATAGRAMS
        {
            return Err(Error::InvalidConfiguration);
        }
        Ok(Self {
            quantum,
            packet_quantum,
            period,
            max_tile,
            max_tile_datagrams,
            generation,
            credit: i64::from(quantum),
            packet_credit: i64::from(packet_quantum),
            last_now: now_ns,
            next_refill: now_ns.checked_add(period).ok_or(Error::ClockOverflow)?,
            serial: 0,
            revision: 0,
            active: None,
            timer: None,
            reserved_tiles: 0,
            charged_bytes: 0,
            charged_datagrams: 0,
        })
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            generation: self.generation,
            credit_bytes: self.credit,
            credit_datagrams: self.packet_credit,
            next_refill_ns: self.next_refill,
            timer: self.timer,
            reserved_tiles: self.reserved_tiles,
            charged_bytes: self.charged_bytes,
            charged_datagrams: self.charged_datagrams,
        }
    }

    fn advance(&mut self, now: u64) -> Result<(), Error> {
        if now < self.last_now {
            return Err(Error::BackwardsClock);
        }
        if now >= self.next_refill {
            let periods = ((now - self.next_refill) / self.period)
                .checked_add(1)
                .ok_or(Error::ClockOverflow)?;
            let next = self
                .next_refill
                .checked_add(
                    periods
                        .checked_mul(self.period)
                        .ok_or(Error::ClockOverflow)?,
                )
                .ok_or(Error::ClockOverflow)?;
            let credit = i128::from(self.credit) + i128::from(periods) * i128::from(self.quantum);
            let packets = i128::from(self.packet_credit)
                + i128::from(periods) * i128::from(self.packet_quantum);
            self.credit = credit.min(i128::from(self.quantum)) as i64;
            self.packet_credit = packets.min(i128::from(self.packet_quantum)) as i64;
            self.next_refill = next;
        }
        self.last_now = now;
        Ok(())
    }

    fn next_serial(&mut self) -> Result<u64, Error> {
        self.serial = self.serial.checked_add(1).ok_or(Error::CounterOverflow)?;
        Ok(self.serial)
    }

    fn wake(&mut self, now: u64, generation: u64) -> Result<WakeHandle, Error> {
        if generation != self.generation {
            return Err(Error::WrongGeneration);
        }
        if self.active.is_some() {
            return Err(Error::ActiveWake);
        }
        self.advance(now)?;
        let handle = WakeHandle {
            generation,
            serial: self.next_serial()?,
        };
        self.active = Some(handle);
        Ok(handle)
    }

    pub fn on_tick(&mut self, now: u64, generation: u64) -> Result<WakeResult, Error> {
        Ok(WakeResult {
            handle: Some(self.wake(now, generation)?),
            encode_slot: true,
        })
    }

    fn arm(&mut self, due: u64) -> Result<(), Error> {
        if self.timer.is_some_and(|old| old.due_ns == due) {
            return Ok(());
        }
        self.timer = Some(Timer {
            generation: self.generation,
            serial: self.next_serial()?,
            due_ns: due,
        });
        Ok(())
    }

    /// One fixed coalesced readiness deadline. More notifications cannot slide
    /// an existing earlier deadline or create additional timer entries.
    pub fn notify_ready(&mut self, now: u64, generation: u64) -> Result<bool, Error> {
        if generation != self.generation {
            return Ok(false);
        }
        if self.active.is_some() {
            return Err(Error::ActiveWake);
        }
        self.advance(now)?;
        let due = now.checked_add(self.period).ok_or(Error::ClockOverflow)?;
        if self.timer.is_none_or(|timer| due < timer.due_ns) {
            self.arm(due)?;
        }
        Ok(true)
    }

    pub fn on_timer(&mut self, now: u64, timer: Timer) -> Result<WakeResult, Error> {
        if self.timer != Some(timer) || timer.generation != self.generation || now < timer.due_ns {
            return Ok(WakeResult {
                handle: None,
                encode_slot: false,
            });
        }
        let handle = self.wake(now, timer.generation)?;
        self.timer = None;
        Ok(WakeResult {
            handle: Some(handle),
            encode_slot: false,
        })
    }

    fn check_wake(&self, wake: WakeHandle) -> Result<(), Error> {
        if self.active != Some(wake) || wake.generation != self.generation {
            return Err(Error::StaleWake);
        }
        Ok(())
    }

    fn required_credit(&self, charge: TileCharge) -> Result<(i64, i64), Error> {
        if charge.bytes == 0
            || charge.bytes > self.max_tile
            || charge.datagrams == 0
            || charge.datagrams > self.max_tile_datagrams
        {
            return Err(Error::InvalidTile);
        }
        // A bounded oversized tile requires a completely unspent quantum.
        // Its full charge creates debt rather than splitting its fragments.
        Ok((
            i64::from(charge.bytes.min(self.quantum)),
            i64::from(charge.datagrams.min(self.packet_quantum)),
        ))
    }

    fn retry_at(&self, charge: TileCharge) -> Result<u64, Error> {
        let (required, packets) = self.required_credit(charge)?;
        if self.credit >= required && self.packet_credit >= packets {
            return Err(Error::WrongBudgetWait);
        }
        let missing = required.saturating_sub(self.credit).max(0) as u64;
        let packet_missing = packets.saturating_sub(self.packet_credit).max(0) as u64;
        let refills = missing
            .div_ceil(u64::from(self.quantum))
            .max(packet_missing.div_ceil(u64::from(self.packet_quantum)));
        self.next_refill
            .checked_add(
                (refills - 1)
                    .checked_mul(self.period)
                    .ok_or(Error::ClockOverflow)?,
            )
            .ok_or(Error::ClockOverflow)
    }

    /// No clock advance occurs within a flush. A long flush cannot refill its
    /// own budget or loop through missed slots without returning to the actor.
    pub fn classify(
        &self,
        wake: WakeHandle,
        charge: TileCharge,
        quinn_space: usize,
    ) -> Result<Admission, Error> {
        self.check_wake(wake)?;
        let (needed, packets) = self.required_credit(charge)?;
        if quinn_space < charge.bytes as usize {
            return Ok(Admission::QuinnBlocked);
        }
        if self.credit < needed || self.packet_credit < packets {
            return Ok(Admission::BudgetBlocked(BudgetWait {
                wake,
                revision: self.revision,
                charge,
                due_ns: self.retry_at(charge)?,
            }));
        }
        Ok(Admission::Ready(Ticket {
            wake,
            revision: self.revision,
            charge,
        }))
    }

    /// Call after validating/removing the exact whole tile, before its first
    /// fragment is handed to Quinn. Never refund after a partial send failure;
    /// that failure closes the session under the existing actor contract.
    pub fn reserve(&mut self, ticket: Ticket) -> Result<(), Error> {
        self.check_wake(ticket.wake)?;
        let (required, packets) = self.required_credit(ticket.charge)?;
        if ticket.revision != self.revision
            || self.credit < required
            || self.packet_credit < packets
        {
            return Err(Error::StaleTicket);
        }
        let revision = self.revision.checked_add(1).ok_or(Error::CounterOverflow)?;
        let tiles = self
            .reserved_tiles
            .checked_add(1)
            .ok_or(Error::CounterOverflow)?;
        let bytes = self
            .charged_bytes
            .checked_add(u64::from(ticket.charge.bytes))
            .ok_or(Error::CounterOverflow)?;
        let datagrams = self
            .charged_datagrams
            .checked_add(u64::from(ticket.charge.datagrams))
            .ok_or(Error::CounterOverflow)?;
        self.credit -= i64::from(ticket.charge.bytes);
        self.packet_credit -= i64::from(ticket.charge.datagrams);
        self.revision = revision;
        self.reserved_tiles = tiles;
        self.charged_bytes = bytes;
        self.charged_datagrams = datagrams;
        Ok(())
    }

    pub fn finish(&mut self, wake: WakeHandle, reason: Stop) -> Result<(), Error> {
        self.check_wake(wake)?;
        match reason {
            Stop::Empty | Stop::QuinnBlocked => self.timer = None,
            Stop::BudgetBlocked(wait) => {
                if wait.wake != wake
                    || wait.revision != self.revision
                    || self.retry_at(wait.charge)? != wait.due_ns
                {
                    return Err(Error::WrongBudgetWait);
                }
                self.arm(wait.due_ns)?;
            }
        }
        self.active = None;
        Ok(())
    }

    /// Resize/resync cancels obsolete work/timer identities, but does not mint
    /// a new quantum, forgive debt, reset the slot phase or revive old tickets.
    pub fn replace_generation(&mut self, now: u64, generation: u64) -> Result<(), Error> {
        if generation == self.generation {
            return Err(Error::WrongGeneration);
        }
        self.advance(now)?;
        let revision = self.revision.checked_add(1).ok_or(Error::CounterOverflow)?;
        self.generation = generation;
        self.active = None;
        self.timer = None;
        self.revision = revision;
        Ok(())
    }
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Selection {
    #[value(name = "16k-16d-1ms")]
    Bounded,
}
impl Selection {
    pub(crate) fn name(self) -> &'static str {
        "16k-16d-1ms"
    }
}

pub(crate) fn selected_name(selected: Option<Selection>) -> &'static str {
    selected.map_or("unlimited", Selection::name)
}

pub(crate) fn configuration(selected: Option<Selection>) -> serde_json::Value {
    match selected {
        None => serde_json::Value::Null,
        Some(value) => {
            serde_json::json!({"mode":value.name(),"charged_bytes_per_slot":QUANTUM_BYTES,
            "application_datagrams_per_slot":QUANTUM_DATAGRAMS,"slot_ns":SLOT_NS,
            "charge_scope":"GLT2 body plus 32-byte header, 8-byte session allowance and 128-byte Quinn metadata per fragment",
            "datagram_scope":"WebTransport application fragments, not physical UDP packets or sk_buff objects",
            "whole_tiles":true,"physical_udp_pacing_guarantee":false})
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum FlushOrigin {
    Tick,
    Timer,
}

#[derive(Clone, Copy)]
pub(crate) struct FlushSummary {
    pub tiles: u64,
    pub datagrams: u64,
    pub application_bytes: u64,
}

pub(crate) struct FlushStart {
    began: std::time::Instant,
    before: Snapshot,
}

#[derive(Default)]
struct Counters {
    started: u64,
    completed: u64,
    tick: u64,
    timer: u64,
    later_slot: u64,
    tiles: u64,
    datagrams: u64,
    application_bytes: u64,
    empty: u64,
    quinn_blocked: u64,
    budget_blocked: u64,
    byte_blocked: u64,
    datagram_blocked: u64,
    generation_cancellations: u64,
    wall_ns: u64,
    max_wall_ns: u64,
    late_ns: u64,
    max_late_ns: u64,
    max_flush_datagrams: u64,
    max_flush_application_bytes: u64,
    max_flush_charge: u64,
    max_slot_datagrams: u64,
    max_slot_charge: u64,
}

pub(crate) struct Sender {
    mode: Selection,
    origin: tokio::time::Instant,
    gate: Quantum,
    counters: Counters,
    last_slot: Option<u64>,
    slot_datagrams: u64,
    slot_charge: u64,
    last_budget_retry: Option<u64>,
}

fn add(value: &mut u64, amount: u64) -> anyhow::Result<()> {
    *value = value.checked_add(amount).ok_or(Error::CounterOverflow)?;
    Ok(())
}

impl Sender {
    pub(crate) fn new(mode: Selection, generation: u64) -> anyhow::Result<Self> {
        Self::at(mode, generation, tokio::time::Instant::now())
    }

    pub(crate) fn at(
        mode: Selection,
        generation: u64,
        origin: tokio::time::Instant,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            mode,
            origin,
            gate: Quantum::new(0, generation)?,
            counters: Counters::default(),
            last_slot: None,
            slot_datagrams: 0,
            slot_charge: 0,
            last_budget_retry: None,
        })
    }

    fn now_ns(&self, now: tokio::time::Instant) -> anyhow::Result<u64> {
        let elapsed = now
            .checked_duration_since(self.origin)
            .ok_or(Error::BackwardsClock)?;
        Ok(elapsed
            .as_nanos()
            .try_into()
            .map_err(|_| Error::ClockOverflow)?)
    }

    fn generation_at(&mut self, now: u64, generation: u64) -> anyhow::Result<()> {
        if self.gate.snapshot().generation != generation {
            self.gate.replace_generation(now, generation)?;
            add(&mut self.counters.generation_cancellations, 1)?;
        }
        Ok(())
    }

    pub(crate) fn invalidate(&mut self, generation: u64) -> anyhow::Result<()> {
        let now = self.now_ns(tokio::time::Instant::now())?;
        self.generation_at(now, generation)
    }

    pub(crate) fn tick_at(
        &mut self,
        now: tokio::time::Instant,
        generation: u64,
    ) -> anyhow::Result<WakeResult> {
        let now = self.now_ns(now)?;
        self.generation_at(now, generation)?;
        Ok(self.gate.on_tick(now, generation)?)
    }

    pub(crate) fn notify_at(
        &mut self,
        now: tokio::time::Instant,
        generation: u64,
    ) -> anyhow::Result<()> {
        let now = self.now_ns(now)?;
        self.generation_at(now, generation)?;
        self.gate.notify_ready(now, generation)?;
        Ok(())
    }

    pub(crate) fn timer(&self) -> Option<Timer> {
        self.gate.snapshot().timer
    }

    pub(crate) fn deadline(&self) -> anyhow::Result<Option<tokio::time::Instant>> {
        self.timer()
            .map(|timer| {
                self.origin
                    .checked_add(std::time::Duration::from_nanos(timer.due_ns()))
                    .ok_or_else(|| Error::ClockOverflow.into())
            })
            .transpose()
    }

    pub(crate) fn timer_at(
        &mut self,
        now: tokio::time::Instant,
        timer: Timer,
        generation: u64,
    ) -> anyhow::Result<WakeResult> {
        let now = self.now_ns(now)?;
        self.generation_at(now, generation)?;
        let result = self.gate.on_timer(now, timer)?;
        if result.handle.is_some() {
            let late = now.saturating_sub(timer.due_ns());
            add(&mut self.counters.late_ns, late)?;
            self.counters.max_late_ns = self.counters.max_late_ns.max(late);
        }
        Ok(result)
    }

    pub(crate) fn classify(
        &self,
        wake: WakeHandle,
        charge: TileCharge,
        space: usize,
    ) -> anyhow::Result<Admission> {
        Ok(self.gate.classify(wake, charge, space)?)
    }

    pub(crate) fn reserve(&mut self, ticket: Ticket) -> anyhow::Result<()> {
        Ok(self.gate.reserve(ticket)?)
    }

    pub(crate) fn begin_flush(
        &mut self,
        wake: WakeHandle,
        origin: FlushOrigin,
    ) -> anyhow::Result<FlushStart> {
        let began = std::time::Instant::now();
        self.gate.check_wake(wake)?;
        let before = self.gate.snapshot();
        add(&mut self.counters.started, 1)?;
        match origin {
            FlushOrigin::Tick => add(&mut self.counters.tick, 1)?,
            FlushOrigin::Timer => add(&mut self.counters.timer, 1)?,
        }
        let slot = self.gate.last_now / SLOT_NS;
        if self.last_slot != Some(slot) {
            if self.last_slot.is_some() {
                add(&mut self.counters.later_slot, 1)?;
            }
            self.last_slot = Some(slot);
            self.slot_datagrams = 0;
            self.slot_charge = 0;
        }
        Ok(FlushStart { began, before })
    }

    pub(crate) fn finish_flush(
        &mut self,
        wake: WakeHandle,
        reason: Stop,
        summary: FlushSummary,
        started: FlushStart,
    ) -> anyhow::Result<()> {
        let after = self.gate.snapshot();
        let charge = summary
            .application_bytes
            .checked_add(
                summary
                    .datagrams
                    .checked_mul(8 + 128)
                    .ok_or(Error::CounterOverflow)?,
            )
            .ok_or(Error::CounterOverflow)?;
        anyhow::ensure!(
            after
                .reserved_tiles
                .checked_sub(started.before.reserved_tiles)
                == Some(summary.tiles)
                && after
                    .charged_datagrams
                    .checked_sub(started.before.charged_datagrams)
                    == Some(summary.datagrams)
                && after
                    .charged_bytes
                    .checked_sub(started.before.charged_bytes)
                    == Some(charge),
            "whole-tile quantum reservations differ from actual admitted fragment counters"
        );
        match reason {
            Stop::Empty => add(&mut self.counters.empty, 1)?,
            Stop::QuinnBlocked => add(&mut self.counters.quinn_blocked, 1)?,
            Stop::BudgetBlocked(wait) => {
                add(&mut self.counters.budget_blocked, 1)?;
                if after.credit_bytes < i64::from(wait.charge.bytes.min(QUANTUM_BYTES)) {
                    add(&mut self.counters.byte_blocked, 1)?;
                }
                if after.credit_datagrams < i64::from(wait.charge.datagrams.min(QUANTUM_DATAGRAMS))
                {
                    add(&mut self.counters.datagram_blocked, 1)?;
                }
                self.last_budget_retry = Some(wait.due_ns());
            }
        }
        self.gate.finish(wake, reason)?;
        add(&mut self.counters.completed, 1)?;
        add(&mut self.counters.tiles, summary.tiles)?;
        add(&mut self.counters.datagrams, summary.datagrams)?;
        add(
            &mut self.counters.application_bytes,
            summary.application_bytes,
        )?;
        add(&mut self.slot_datagrams, summary.datagrams)?;
        add(&mut self.slot_charge, charge)?;
        self.counters.max_slot_datagrams =
            self.counters.max_slot_datagrams.max(self.slot_datagrams);
        self.counters.max_slot_charge = self.counters.max_slot_charge.max(self.slot_charge);
        self.counters.max_flush_datagrams =
            self.counters.max_flush_datagrams.max(summary.datagrams);
        self.counters.max_flush_application_bytes = self
            .counters
            .max_flush_application_bytes
            .max(summary.application_bytes);
        self.counters.max_flush_charge = self.counters.max_flush_charge.max(charge);
        let elapsed: u64 = started
            .began
            .elapsed()
            .as_nanos()
            .try_into()
            .map_err(|_| Error::ClockOverflow)?;
        add(&mut self.counters.wall_ns, elapsed)?;
        self.counters.max_wall_ns = self.counters.max_wall_ns.max(elapsed);
        Ok(())
    }

    pub(crate) fn snapshot(&self) -> serde_json::Value {
        let gate = self.gate.snapshot();
        let c = &self.counters;
        serde_json::json!({"mode":self.mode.name(),"configuration":configuration(Some(self.mode)),
            "whole_flush_calls_started":c.started,"whole_flush_calls_completed":c.completed,
            "tick_flush_calls":c.tick,"timer_flush_calls":c.timer,"later_slot_flush_calls":c.later_slot,
            "admitted_tiles":c.tiles,"application_datagrams_enqueued":c.datagrams,"application_datagram_bytes":c.application_bytes,
            "reserved_whole_tiles":gate.reserved_tiles,"charged_bytes_reserved":gate.charged_bytes,"datagrams_reserved":gate.charged_datagrams,
            "empty_flushes":c.empty,"quinn_blocked_flushes":c.quinn_blocked,"budget_blocked_flushes":c.budget_blocked,
            "byte_budget_blocks":c.byte_blocked,"datagram_budget_blocks":c.datagram_blocked,
            "generation_cancellations":c.generation_cancellations,"budget_generation":gate.generation,
            "remaining_charge_bytes":gate.credit_bytes,"remaining_datagrams":gate.credit_datagrams,
            "next_refill_elapsed_ns":gate.next_refill_ns,
            "pending_deadline_elapsed_ns":gate.timer.map(Timer::due_ns),"pending_deadline_generation":gate.timer.map(Timer::generation),
            "last_budget_retry_elapsed_ns":self.last_budget_retry,"flush_wall_ns_total":c.wall_ns,"flush_wall_ns_max":c.max_wall_ns,
            "timer_lateness_ns_total":c.late_ns,"timer_lateness_ns_max":c.max_late_ns,
            "max_flush_application_datagrams":c.max_flush_datagrams,"max_flush_application_bytes":c.max_flush_application_bytes,
            "max_flush_charge_bytes":c.max_flush_charge,"max_budget_slot_application_datagrams":c.max_slot_datagrams,
            "max_budget_slot_charge_bytes":c.max_slot_charge,"scope":"Application admission; budget slots assigned at wake. Not physical UDP pacing or sk_buff accounting."})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wake(gate: &mut Quantum, now: u64) -> WakeHandle {
        let generation = gate.snapshot().generation;
        let result = gate.on_tick(now, generation).unwrap();
        assert!(result.encode_slot);
        result.handle.unwrap()
    }
    fn charge(bytes: u32, datagrams: u32) -> TileCharge {
        TileCharge { bytes, datagrams }
    }
    fn reserve(gate: &mut Quantum, handle: WakeHandle, tile: TileCharge) -> Ticket {
        let Admission::Ready(ticket) = gate.classify(handle, tile, usize::MAX).unwrap() else {
            panic!("whole tile was blocked")
        };
        gate.reserve(ticket).unwrap();
        ticket
    }
    fn wait(gate: &Quantum, handle: WakeHandle, tile: TileCharge) -> BudgetWait {
        let Admission::BudgetBlocked(wait) = gate.classify(handle, tile, usize::MAX).unwrap()
        else {
            panic!("budget did not block")
        };
        wait
    }
    fn smaller_quanta(bytes: u32, datagrams: u32) -> Limits {
        Limits {
            quantum_bytes: bytes,
            quantum_datagrams: datagrams,
            ..Limits::default()
        }
    }

    #[test]
    fn exact_current_charge_includes_all_fragments_without_raising_body_or_payload_limits() {
        for stride in [1024, 1100, 1168] {
            assert_eq!(current_tile_charge(4096, stride).unwrap(), charge(4768, 4));
            assert_eq!(current_tile_charge(4, stride).unwrap(), charge(172, 1));
        }
        const {
            assert!(CURRENT_MAX_TILE_CHARGE < QUANTUM_BYTES);
        }
        for (body, stride) in [(0, 1024), (4097, 1024), (4096, 1023), (4096, 1169)] {
            assert_eq!(current_tile_charge(body, stride), Err(Error::InvalidTile));
        }
    }

    #[test]
    fn tiny_datagrams_hit_the_packet_limit_while_byte_credit_remains() {
        let mut gate = Quantum::new(0, 1).unwrap();
        let handle = wake(&mut gate, 0);
        let tiny = charge(172, 1);
        for _ in 0..16 {
            reserve(&mut gate, handle, tiny);
        }
        assert_eq!(gate.snapshot().credit_datagrams, 0);
        assert_eq!(gate.snapshot().credit_bytes, 13632);
        let blocked = wait(&gate, handle, tiny);
        assert_eq!(blocked.due_ns(), SLOT_NS);
        gate.finish(handle, Stop::BudgetBlocked(blocked)).unwrap();
        let timer = gate.snapshot().timer.unwrap();
        let result = gate.on_timer(SLOT_NS, timer).unwrap();
        assert!(!result.encode_slot);
        reserve(&mut gate, result.handle.unwrap(), tiny);
        assert_eq!(gate.snapshot().charged_datagrams, 17);
    }

    #[test]
    fn whole_raw_tiles_hit_byte_limit_without_splitting_or_skipping_the_head() {
        let mut gate = Quantum::new(0, 1).unwrap();
        let handle = wake(&mut gate, 0);
        let raw = charge(4768, 4);
        for _ in 0..3 {
            reserve(&mut gate, handle, raw);
        }
        assert_eq!(gate.snapshot().credit_bytes, 2080);
        assert_eq!(gate.snapshot().credit_datagrams, 4);
        let blocked = wait(&gate, handle, raw);
        assert_eq!(blocked.due_ns(), SLOT_NS);
        gate.finish(handle, Stop::BudgetBlocked(blocked)).unwrap();
        assert_eq!(gate.snapshot().reserved_tiles, 3);
        assert_eq!(gate.snapshot().charged_bytes, 14304);
    }

    #[test]
    fn tick_and_ready_share_remaining_budgets_and_ready_never_encodes() {
        let mut gate = Quantum::new(0, 1).unwrap();
        gate.notify_ready(0, 1).unwrap();
        let timer = gate.snapshot().timer.unwrap();
        let first = gate.on_timer(SLOT_NS, timer).unwrap();
        assert!(!first.encode_slot);
        let handle = first.handle.unwrap();
        reserve(&mut gate, handle, charge(4000, 4));
        gate.finish(handle, Stop::Empty).unwrap();
        let tick = wake(&mut gate, SLOT_NS + 1);
        assert_eq!(gate.snapshot().credit_bytes, 12384);
        assert_eq!(gate.snapshot().credit_datagrams, 12);
        for _ in 0..3 {
            reserve(&mut gate, tick, charge(4000, 4));
        }
        assert_eq!(gate.snapshot().credit_bytes, 384);
        assert_eq!(gate.snapshot().credit_datagrams, 0);
        assert_eq!(wait(&gate, tick, charge(172, 1)).due_ns(), 2 * SLOT_NS);
    }

    #[test]
    fn readiness_is_one_fixed_deadline_and_cannot_refill_an_active_flush() {
        let mut gate = Quantum::new(0, 5).unwrap();
        gate.notify_ready(0, 5).unwrap();
        let first = gate.snapshot().timer.unwrap();
        gate.notify_ready(900_000, 5).unwrap();
        assert_eq!(gate.snapshot().timer, Some(first));
        assert_eq!(gate.on_timer(999_999, first).unwrap().handle, None);
        let handle = gate.on_timer(SLOT_NS, first).unwrap().handle.unwrap();
        assert_eq!(gate.notify_ready(100 * SLOT_NS, 5), Err(Error::ActiveWake));
        reserve(&mut gate, handle, charge(4768, 4));
        assert_eq!(gate.snapshot().credit_bytes, 11616);
        gate.finish(handle, Stop::Empty).unwrap();
        assert!(gate.snapshot().timer.is_none());
    }

    #[test]
    fn quinn_block_does_not_spend_or_arm_any_self_retry() {
        let mut gate = Quantum::new(0, 1).unwrap();
        gate.notify_ready(0, 1).unwrap();
        let stale = gate.snapshot().timer.unwrap();
        let handle = wake(&mut gate, 0);
        assert_eq!(
            gate.classify(handle, charge(4768, 4), 4767).unwrap(),
            Admission::QuinnBlocked
        );
        gate.finish(handle, Stop::QuinnBlocked).unwrap();
        assert!(gate.snapshot().timer.is_none());
        assert_eq!(gate.snapshot().reserved_tiles, 0);
        assert_eq!(gate.snapshot().credit_bytes, i64::from(QUANTUM_BYTES));
        assert_eq!(gate.on_timer(SLOT_NS, stale).unwrap().handle, None);
        let tick = wake(&mut gate, 16 * SLOT_NS);
        reserve(&mut gate, tick, charge(4768, 4));
    }

    #[test]
    fn quinn_block_wins_when_transport_and_both_budgets_are_exhausted() {
        let mut gate = Quantum::new(0, 1).unwrap();
        let handle = wake(&mut gate, 0);
        for _ in 0..16 {
            reserve(&mut gate, handle, charge(172, 1));
        }
        assert_eq!(
            gate.classify(handle, charge(172, 1), 0).unwrap(),
            Admission::QuinnBlocked
        );
        gate.finish(handle, Stop::QuinnBlocked).unwrap();
        assert!(gate.snapshot().timer.is_none());
    }

    #[test]
    fn repeated_wakes_do_not_create_credit_and_missed_slots_do_not_accumulate_bursts() {
        let mut gate = Quantum::new(0, 1).unwrap();
        let handle = wake(&mut gate, 0);
        for _ in 0..16 {
            reserve(&mut gate, handle, charge(172, 1));
        }
        let blocked = wait(&gate, handle, charge(172, 1));
        gate.finish(handle, Stop::BudgetBlocked(blocked)).unwrap();
        let timer = gate.snapshot().timer.unwrap();
        let early = wake(&mut gate, 500_000);
        let again = wait(&gate, early, charge(172, 1));
        gate.finish(early, Stop::BudgetBlocked(again)).unwrap();
        assert_eq!(gate.snapshot().timer, Some(timer));
        let late = wake(&mut gate, 100 * SLOT_NS);
        assert_eq!(gate.snapshot().credit_datagrams, 16);
        for _ in 0..16 {
            reserve(&mut gate, late, charge(172, 1));
        }
        assert!(matches!(
            gate.classify(late, charge(172, 1), usize::MAX).unwrap(),
            Admission::BudgetBlocked(_)
        ));
    }

    #[test]
    fn generation_preemption_cancels_old_timers_tickets_and_waits_without_resetting_credit() {
        let mut gate = Quantum::new(0, 1).unwrap();
        let handle = wake(&mut gate, 0);
        let ticket = reserve(&mut gate, handle, charge(4768, 4));
        for _ in 0..2 {
            reserve(&mut gate, handle, charge(4768, 4));
        }
        let blocked = wait(&gate, handle, charge(4768, 4));
        gate.finish(handle, Stop::BudgetBlocked(blocked)).unwrap();
        let old_timer = gate.snapshot().timer.unwrap();
        let before = gate.snapshot();
        for generation in 2..=20 {
            gate.replace_generation(100_000, generation).unwrap();
        }
        assert_eq!(gate.snapshot().credit_bytes, before.credit_bytes);
        assert_eq!(gate.snapshot().credit_datagrams, before.credit_datagrams);
        assert_eq!(gate.snapshot().next_refill_ns, before.next_refill_ns);
        assert!(gate.snapshot().timer.is_none());
        assert!(!gate.notify_ready(100_000, 1).unwrap());
        assert_eq!(gate.on_timer(SLOT_NS, old_timer).unwrap().handle, None);
        assert_eq!(gate.reserve(ticket), Err(Error::StaleWake));
        assert_eq!(
            gate.finish(handle, Stop::BudgetBlocked(blocked)),
            Err(Error::StaleWake)
        );
        let current = wake(&mut gate, 100_000);
        reserve(&mut gate, current, charge(172, 1));
        assert_eq!(wait(&gate, current, charge(4768, 4)).due_ns(), SLOT_NS);
    }

    #[test]
    fn duplicate_reservations_and_old_handles_cannot_double_spend_a_whole_tile() {
        let mut gate = Quantum::new(0, 1).unwrap();
        let handle = wake(&mut gate, 0);
        let ticket = reserve(&mut gate, handle, charge(4768, 4));
        assert_eq!(gate.reserve(ticket), Err(Error::StaleTicket));
        gate.finish(handle, Stop::Empty).unwrap();
        assert_eq!(gate.reserve(ticket), Err(Error::StaleWake));
        let next = wake(&mut gate, 1);
        assert_eq!(
            gate.classify(handle, charge(172, 1), usize::MAX),
            Err(Error::StaleWake)
        );
        reserve(&mut gate, next, charge(172, 1));
        assert_eq!(gate.snapshot().reserved_tiles, 2);
    }

    #[test]
    fn bounded_oversized_byte_tile_is_whole_and_repays_debt_before_future_slots() {
        let mut gate = Quantum::with_config(0, 1, smaller_quanta(1000, 16)).unwrap();
        let handle = wake(&mut gate, 0);
        reserve(&mut gate, handle, charge(2500, 2));
        assert_eq!(gate.snapshot().credit_bytes, -1500);
        let blocked = wait(&gate, handle, charge(500, 1));
        assert_eq!(blocked.due_ns(), 2 * SLOT_NS);
        gate.finish(handle, Stop::BudgetBlocked(blocked)).unwrap();
        let timer = gate.snapshot().timer.unwrap();
        assert_eq!(gate.on_timer(SLOT_NS, timer).unwrap().handle, None);
        let next = gate.on_timer(2 * SLOT_NS, timer).unwrap().handle.unwrap();
        reserve(&mut gate, next, charge(500, 1));
        assert_eq!(gate.snapshot().credit_bytes, 0);
        assert_eq!(gate.snapshot().charged_bytes, 3000);
        let oversized = wait(&gate, next, charge(2500, 2));
        assert_eq!(oversized.due_ns(), 3 * SLOT_NS);
        gate.finish(next, Stop::BudgetBlocked(oversized)).unwrap();
        let timer = gate.snapshot().timer.unwrap();
        let next = gate.on_timer(3 * SLOT_NS, timer).unwrap().handle.unwrap();
        reserve(&mut gate, next, charge(2500, 2));
        assert_eq!(gate.snapshot().credit_bytes, -1500);
    }

    #[test]
    fn bounded_oversized_packet_tile_is_not_starved_or_silently_fragmented_across_slots() {
        let mut gate = Quantum::with_config(0, 1, smaller_quanta(QUANTUM_BYTES, 2)).unwrap();
        let handle = wake(&mut gate, 0);
        reserve(&mut gate, handle, charge(4768, 4));
        assert_eq!(gate.snapshot().credit_datagrams, -2);
        let blocked = wait(&gate, handle, charge(172, 1));
        assert_eq!(blocked.due_ns(), 2 * SLOT_NS);
        gate.finish(handle, Stop::BudgetBlocked(blocked)).unwrap();
        let timer = gate.snapshot().timer.unwrap();
        let next = gate.on_timer(2 * SLOT_NS, timer).unwrap().handle.unwrap();
        reserve(&mut gate, next, charge(4768, 4));
        assert_eq!(gate.snapshot().credit_datagrams, -2);
        assert_eq!(gate.snapshot().charged_datagrams, 8);
    }

    #[test]
    fn dual_dimension_debt_uses_the_later_fixed_refill_and_is_not_forgiven_by_resync() {
        let mut gate = Quantum::with_config(0, 1, smaller_quanta(2000, 1)).unwrap();
        let handle = wake(&mut gate, 0);
        reserve(&mut gate, handle, charge(4768, 4));
        let blocked = wait(&gate, handle, charge(500, 1));
        assert_eq!(blocked.due_ns(), 4 * SLOT_NS);
        gate.finish(handle, Stop::BudgetBlocked(blocked)).unwrap();
        gate.replace_generation(500_000, 2).unwrap();
        assert_eq!(gate.snapshot().credit_bytes, -2768);
        assert_eq!(gate.snapshot().credit_datagrams, -3);
        let next = wake(&mut gate, 500_000);
        assert_eq!(wait(&gate, next, charge(500, 1)).due_ns(), 4 * SLOT_NS);
    }

    #[test]
    fn limits_clocks_and_empty_stop_fail_closed_without_refunding_spent_reservations() {
        assert!(matches!(
            Quantum::new(u64::MAX, 1),
            Err(Error::ClockOverflow)
        ));
        assert!(matches!(
            Quantum::with_config(
                0,
                1,
                Limits {
                    quantum_datagrams: 0,
                    ..Limits::default()
                }
            ),
            Err(Error::InvalidConfiguration)
        ));
        let mut gate = Quantum::new(100, 1).unwrap();
        assert_eq!(gate.on_tick(99, 1), Err(Error::BackwardsClock));
        let handle = wake(&mut gate, 100);
        for bad in [
            charge(0, 1),
            charge(CURRENT_MAX_TILE_CHARGE + 1, 1),
            charge(1, 0),
            charge(172, 5),
        ] {
            assert_eq!(
                gate.classify(handle, bad, usize::MAX),
                Err(Error::InvalidTile)
            );
        }
        reserve(&mut gate, handle, charge(4768, 4));
        gate.finish(handle, Stop::Empty).unwrap();
        assert_eq!(gate.snapshot().credit_bytes, 11616);
        assert_eq!(gate.snapshot().credit_datagrams, 12);
        assert!(gate.snapshot().timer.is_none());
        assert_eq!(gate.snapshot().charged_bytes, 4768);
        assert_eq!(gate.snapshot().charged_datagrams, 4);
    }
}
