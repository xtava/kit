use super::*;
use crate::terminalstate::{performer::Performer, TerminalStateResizeGeometryPlan};
use std::fmt;
use std::mem;
use std::sync::Arc;
use wezterm_escape_parser::parser::Parser;
use wezterm_escape_parser::{Action, OperatingSystemCommand};
use wezterm_runtime_admission::{
    AdmissionError, RetainedClass, RetainedStateLease, MAX_SERVER_TERMINAL_ACTION_BYTES,
    MAX_SERVER_TERMINAL_FIXED_BYTES, MAX_SERVER_TERMINAL_IMAGE_MUTATION_BYTES,
    SERVER_TERMINAL_ACTION_AMPLIFICATION,
};

const MAX_SERVER_TERMINAL_INITIAL_DYNAMIC_BYTES: usize = 65_536;
const MAX_SERVER_TERMINAL_IDENTITY_BYTES: usize = 32_768;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
pub enum ClipboardSelection {
    Clipboard,
    PrimarySelection,
}

pub trait Clipboard: Send + Sync {
    fn set_contents(
        &self,
        selection: ClipboardSelection,
        data: Option<String>,
    ) -> anyhow::Result<()>;
}

impl Clipboard for Box<dyn Clipboard> {
    fn set_contents(
        &self,
        selection: ClipboardSelection,
        data: Option<String>,
    ) -> anyhow::Result<()> {
        self.as_ref().set_contents(selection, data)
    }
}

pub trait DeviceControlHandler: Send + Sync {
    fn handle_device_control(&mut self, _control: wezterm_escape_parser::DeviceControlMode);
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
pub enum Progress {
    #[default]
    None,
    Percentage(u8),
    Error(u8),
    Indeterminate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
pub enum Alert {
    Bell,
    ToastNotification {
        /// The title text for the notification.
        title: Option<String>,
        /// The message body
        body: String,
        /// Whether clicking on the notification should focus the
        /// window/tab/pane that generated it
        focus: bool,
    },
    CurrentWorkingDirectoryChanged,
    IconTitleChanged(Option<String>),
    WindowTitleChanged(String),
    TabTitleChanged(Option<String>),
    /// When the color palette has been updated
    PaletteChanged,
    /// A UserVar has changed value
    SetUserVar {
        name: String,
        value: String,
    },
    /// When something bumps the seqno in the terminal model and
    /// the terminal is not focused
    OutputSinceFocusLost,
    /// A change to the progress bar state
    Progress(Progress),
}

pub trait AlertHandler: Send + Sync {
    fn alert(&mut self, alert: Alert);
}

pub trait DownloadHandler: Send + Sync {
    fn save_to_downloads(&self, name: Option<String>, data: Vec<u8>);
}

/// Represents an instance of a terminal emulator.
pub struct Terminal {
    /// The terminal model/state
    state: TerminalState,
    /// Baseline terminal escape sequence parser
    parser: Parser,
    /// Stable identity used to bind bounded resize plans to this terminal instance.
    bounded_terminal_identity: Option<Arc<()>>,
    server_retained: Option<ServerTerminalRetainedState>,
    parser_retained_upper_bound: usize,
}

#[derive(Debug)]
struct ServerTerminalRetainedState {
    lease: RetainedStateLease,
    geometry_limits: TerminalGeometryLimits,
}

impl Deref for Terminal {
    type Target = TerminalState;

    fn deref(&self) -> &TerminalState {
        &self.state
    }
}

impl DerefMut for Terminal {
    fn deref_mut(&mut self) -> &mut TerminalState {
        &mut self.state
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, FromDynamic, ToDynamic)]
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
pub struct TerminalSize {
    pub rows: usize,
    pub cols: usize,
    pub pixel_width: usize,
    pub pixel_height: usize,
    pub dpi: u32,
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
            dpi: 0,
        }
    }
}

/// Caller-supplied limits for constructing a bounded terminal geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalGeometryLimits {
    pub max_rows: usize,
    pub max_cols: usize,
    pub max_pixel_width: usize,
    pub max_pixel_height: usize,
    pub max_scrollback_rows: usize,
    pub max_geometry_bytes: usize,
}

/// Why a terminal geometry could not be safely planned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalGeometryError {
    ZeroDimension {
        dimension: &'static str,
    },
    DimensionExceedsLimit {
        dimension: &'static str,
        actual: usize,
        limit: usize,
    },
    PtyFieldOverflow {
        field: &'static str,
        actual: usize,
        maximum: usize,
    },
    ConfiguredScrollbackRowsExceedLimit {
        actual: usize,
        limit: usize,
    },
    ArithmeticOverflow {
        calculation: &'static str,
    },
    GeometryBytesExceedLimit {
        required: usize,
        limit: usize,
    },
    ResizeRequiresBoundedTerminal,
    ResizePlanTerminalMismatch,
    StaleResizePlan {
        planned_epoch: SequenceNo,
        current_epoch: SequenceNo,
    },
    ResizePlanGeometryMutated {
        planned_epoch: u64,
        current_epoch: u64,
    },
    UnplannedBoundedResize,
}

#[derive(Debug)]
pub enum TerminalRetainedStateError {
    Geometry(TerminalGeometryError),
    Admission(AdmissionError),
    ActionBatchTooLarge { actual: usize, maximum: usize },
    InitialIdentityTooLarge { actual: usize, maximum: usize },
    RetainedStateOverflow { calculation: &'static str },
    RetainedStateInvariant { measured: usize, reserved: usize },
}

impl fmt::Display for TerminalRetainedStateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Geometry(error) => error.fmt(formatter),
            Self::Admission(error) => error.fmt(formatter),
            Self::ActionBatchTooLarge { actual, maximum } => write!(
                formatter,
                "terminal action batch of {actual} bytes exceeds maximum of {maximum}"
            ),
            Self::InitialIdentityTooLarge { actual, maximum } => write!(
                formatter,
                "terminal program and version require {actual} bytes, exceeding {maximum}"
            ),
            Self::RetainedStateOverflow { calculation } => {
                write!(
                    formatter,
                    "terminal retained-state overflow while calculating {calculation}"
                )
            }
            Self::RetainedStateInvariant { measured, reserved } => write!(
                formatter,
                "terminal retained state measured {measured} bytes after reserving {reserved}"
            ),
        }
    }
}

impl std::error::Error for TerminalRetainedStateError {}

impl From<TerminalGeometryError> for TerminalRetainedStateError {
    fn from(error: TerminalGeometryError) -> Self {
        Self::Geometry(error)
    }
}

impl From<AdmissionError> for TerminalRetainedStateError {
    fn from(error: AdmissionError) -> Self {
        Self::Admission(error)
    }
}

impl fmt::Display for TerminalGeometryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroDimension { dimension } => {
                write!(f, "terminal {dimension} must be greater than zero")
            }
            Self::DimensionExceedsLimit {
                dimension,
                actual,
                limit,
            } => write!(
                f,
                "terminal {dimension} {actual} exceeds the configured limit {limit}"
            ),
            Self::PtyFieldOverflow {
                field,
                actual,
                maximum,
            } => write!(
                f,
                "terminal {field} {actual} exceeds the PTY field maximum {maximum}"
            ),
            Self::ConfiguredScrollbackRowsExceedLimit { actual, limit } => write!(
                f,
                "configured terminal scrollback rows {actual} exceeds the limit {limit}"
            ),
            Self::ArithmeticOverflow { calculation } => {
                write!(
                    f,
                    "terminal geometry overflow while calculating {calculation}"
                )
            }
            Self::GeometryBytesExceedLimit { required, limit } => write!(
                f,
                "terminal geometry requires {required} bytes, exceeding the limit {limit}"
            ),
            Self::ResizeRequiresBoundedTerminal => {
                write!(f, "bounded resize requires a fixed-scrollback terminal")
            }
            Self::ResizePlanTerminalMismatch => {
                write!(f, "bounded resize plan belongs to a different terminal")
            }
            Self::StaleResizePlan {
                planned_epoch,
                current_epoch,
            } => write!(
                f,
                "bounded resize plan epoch {planned_epoch} is stale; current epoch is {current_epoch}"
            ),
            Self::ResizePlanGeometryMutated {
                planned_epoch,
                current_epoch,
            } => write!(
                f,
                "bounded resize geometry epoch {planned_epoch} is stale; current epoch is {current_epoch}"
            ),
            Self::UnplannedBoundedResize => {
                write!(f, "fixed-scrollback terminals require a bounded resize plan")
            }
        }
    }
}

impl std::error::Error for TerminalGeometryError {}

/// A validated, opaque construction token for a bounded terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalConstructionGeometryPlan {
    size: TerminalSize,
    configured_scrollback_rows: usize,
    geometry_bytes: usize,
}

impl TerminalConstructionGeometryPlan {
    pub fn size(&self) -> TerminalSize {
        self.size
    }

    pub fn configured_scrollback_rows(&self) -> usize {
        self.configured_scrollback_rows
    }

    pub fn geometry_bytes(&self) -> usize {
        self.geometry_bytes
    }

    pub fn initial_server_retained_bytes(&self) -> Result<usize, TerminalRetainedStateError> {
        self.geometry_bytes
            .checked_add(MAX_SERVER_TERMINAL_FIXED_BYTES)
            .and_then(|bytes| bytes.checked_add(MAX_SERVER_TERMINAL_INITIAL_DYNAMIC_BYTES))
            .ok_or(TerminalRetainedStateError::RetainedStateOverflow {
                calculation: "initial server terminal retained state",
            })
    }
}

/// A state-bound, opaque plan for one bounded terminal resize.
#[derive(Debug)]
pub struct TerminalResizeGeometryPlan {
    terminal_identity: Arc<()>,
    epoch: SequenceNo,
    geometry_mutation_epoch: u64,
    target: TerminalSize,
    current_geometry_retained_bytes: usize,
    settled_geometry_retained_upper_bound: usize,
    peak_geometry_bytes: usize,
    additional_bytes_required: usize,
    state_plan: TerminalStateResizeGeometryPlan,
}

pub struct PreparedServerTerminalResize<'a> {
    terminal: &'a mut Terminal,
    plan: Option<TerminalResizeGeometryPlan>,
    prior_retained_bytes: usize,
    settled_retained_bytes: usize,
    committed: bool,
}

impl PreparedServerTerminalResize<'_> {
    pub fn target_size(&self) -> TerminalSize {
        self.plan
            .as_ref()
            .expect("resize plan already consumed")
            .target_size()
    }

    pub fn commit(mut self) {
        let plan = self.plan.take().expect("resize plan already consumed");
        self.terminal
            .state
            .apply_bounded_resize(plan.target, plan.state_plan);
        self.terminal
            .server_retained
            .as_mut()
            .expect("prepared server resize lost its retained-state owner")
            .lease
            .try_resize(self.settled_retained_bytes)
            .expect("settling a pre-admitted terminal resize only shrinks its lease");
        self.committed = true;
    }
}

impl Drop for PreparedServerTerminalResize<'_> {
    fn drop(&mut self) {
        if self.committed || self.plan.is_none() {
            return;
        }
        if let Some(retained) = self.terminal.server_retained.as_mut() {
            retained
                .lease
                .try_resize(self.prior_retained_bytes)
                .expect("shrinking a cancelled terminal resize lease cannot fail");
        }
    }
}

impl TerminalResizeGeometryPlan {
    pub fn target_size(&self) -> TerminalSize {
        self.target
    }

    pub fn current_geometry_retained_bytes(&self) -> usize {
        self.current_geometry_retained_bytes
    }

    pub fn settled_geometry_retained_upper_bound(&self) -> usize {
        self.settled_geometry_retained_upper_bound
    }

    pub fn peak_geometry_bytes(&self) -> usize {
        self.peak_geometry_bytes
    }

    pub fn additional_bytes_required(&self) -> usize {
        self.additional_bytes_required
    }

    #[cfg(test)]
    pub(crate) fn primary_line_capacity_request(&self) -> usize {
        self.state_plan.primary_line_capacity_request()
    }
}

fn validate_dimension(
    dimension: &'static str,
    actual: usize,
    limit: usize,
) -> Result<(), TerminalGeometryError> {
    if actual > limit {
        return Err(TerminalGeometryError::DimensionExceedsLimit {
            dimension,
            actual,
            limit,
        });
    }
    Ok(())
}

fn validate_pty_field(field: &'static str, actual: usize) -> Result<(), TerminalGeometryError> {
    let maximum = u16::MAX as usize;
    if actual > maximum {
        return Err(TerminalGeometryError::PtyFieldOverflow {
            field,
            actual,
            maximum,
        });
    }
    Ok(())
}

/// Conservatively bounds collection capacity rounding during construction.
pub(crate) fn conservative_collection_capacity(
    requested: usize,
    calculation: &'static str,
) -> Result<usize, TerminalGeometryError> {
    if requested == 0 {
        return Ok(0);
    }

    requested
        .checked_next_power_of_two()
        .and_then(|capacity| capacity.checked_mul(2))
        .ok_or(TerminalGeometryError::ArithmeticOverflow { calculation })
}

pub(crate) fn checked_geometry_add(
    total: usize,
    value: usize,
    calculation: &'static str,
) -> Result<usize, TerminalGeometryError> {
    total
        .checked_add(value)
        .ok_or(TerminalGeometryError::ArithmeticOverflow { calculation })
}

pub(crate) fn checked_geometry_mul(
    left: usize,
    right: usize,
    calculation: &'static str,
) -> Result<usize, TerminalGeometryError> {
    left.checked_mul(right)
        .ok_or(TerminalGeometryError::ArithmeticOverflow { calculation })
}

impl Terminal {
    /// Validate all geometry allocations required by bounded terminal construction.
    ///
    /// This performs no allocation and starts no writer. The returned plan captures the
    /// configured scrollback row count so bounded construction cannot later expand it from a
    /// reloaded configuration.
    pub fn plan_bounded_construction(
        size: TerminalSize,
        configured_scrollback_rows: usize,
        limits: TerminalGeometryLimits,
    ) -> Result<TerminalConstructionGeometryPlan, TerminalGeometryError> {
        if size.rows == 0 {
            return Err(TerminalGeometryError::ZeroDimension { dimension: "rows" });
        }
        if size.cols == 0 {
            return Err(TerminalGeometryError::ZeroDimension {
                dimension: "columns",
            });
        }

        validate_dimension("rows", size.rows, limits.max_rows)?;
        validate_dimension("columns", size.cols, limits.max_cols)?;
        validate_dimension("pixel width", size.pixel_width, limits.max_pixel_width)?;
        validate_dimension("pixel height", size.pixel_height, limits.max_pixel_height)?;

        if configured_scrollback_rows > limits.max_scrollback_rows {
            return Err(TerminalGeometryError::ConfiguredScrollbackRowsExceedLimit {
                actual: configured_scrollback_rows,
                limit: limits.max_scrollback_rows,
            });
        }

        validate_pty_field("rows", size.rows)?;
        validate_pty_field("columns", size.cols)?;
        validate_pty_field("pixel width", size.pixel_width)?;
        validate_pty_field("pixel height", size.pixel_height)?;

        let primary_requested_lines = size.rows.checked_add(configured_scrollback_rows).ok_or(
            TerminalGeometryError::ArithmeticOverflow {
                calculation: "primary rows plus scrollback",
            },
        )?;
        let primary_line_capacity = conservative_collection_capacity(
            primary_requested_lines,
            "primary screen line capacity",
        )?;
        let alternate_line_capacity =
            conservative_collection_capacity(size.rows, "alternate screen line capacity")?;
        let total_line_capacity = primary_line_capacity
            .checked_add(alternate_line_capacity)
            .ok_or(TerminalGeometryError::ArithmeticOverflow {
                calculation: "primary plus alternate screen line capacity",
            })?;
        let line_slot_bytes = checked_geometry_mul(
            total_line_capacity,
            mem::size_of::<Line>(),
            "screen line slot bytes",
        )?;

        let blank_line_count =
            size.rows
                .checked_mul(2)
                .ok_or(TerminalGeometryError::ArithmeticOverflow {
                    calculation: "primary plus alternate blank line count",
                })?;
        let blank_line_heap_bytes = checked_geometry_mul(
            blank_line_count,
            Line::INITIAL_CLUSTER_TEXT_CAPACITY,
            "blank line clustered text heap bytes",
        )?;

        // Vec<bool> stores bits in machine words. Counting one byte per conservative logical
        // slot is an overestimate except for very small vectors, where one machine word is the
        // conservative minimum.
        let conservative_tab_slots =
            conservative_collection_capacity(size.cols, "tab stop capacity")?;
        let tab_storage_bytes = conservative_tab_slots.max(mem::size_of::<usize>());

        let geometry_bytes = checked_geometry_add(
            line_slot_bytes,
            blank_line_heap_bytes,
            "line slots plus blank line heap bytes",
        )?;
        let geometry_bytes = checked_geometry_add(
            geometry_bytes,
            tab_storage_bytes,
            "screen plus tab storage bytes",
        )?;

        if geometry_bytes > limits.max_geometry_bytes {
            return Err(TerminalGeometryError::GeometryBytesExceedLimit {
                required: geometry_bytes,
                limit: limits.max_geometry_bytes,
            });
        }

        Ok(TerminalConstructionGeometryPlan {
            size,
            configured_scrollback_rows,
            geometry_bytes,
        })
    }

    /// Construct a new Terminal.
    /// `physical_rows` and `physical_cols` describe the dimensions
    /// of the visible portion of the terminal display in terms of
    /// the number of text cells.
    ///
    /// `pixel_width` and `pixel_height` describe the dimensions of
    /// that same visible area but in pixels.
    ///
    /// `term_program` and `term_version` are required to identify
    /// the host terminal program; they are used to respond to the
    /// terminal identification sequence `\033[>q`.
    ///
    /// `writer` is anything that implements `std::io::Write`; it
    /// is used to send input to the connected program; both keyboard
    /// and mouse input is encoded and written to that stream, as
    /// are answerback responses to a number of escape sequences.
    pub fn new(
        size: TerminalSize,
        config: Arc<dyn TerminalConfiguration + Send + Sync>,
        term_program: &str,
        term_version: &str,
        // writing to the writer sends data to input of the pty
        writer: Box<dyn std::io::Write + Send>,
    ) -> Terminal {
        Terminal {
            state: TerminalState::new(size, config, term_program, term_version, writer),
            parser: Parser::new(),
            bounded_terminal_identity: None,
            server_retained: None,
            parser_retained_upper_bound: 0,
        }
    }

    /// Construct a terminal using a previously validated bounded geometry plan.
    pub fn new_from_geometry_plan(
        plan: TerminalConstructionGeometryPlan,
        config: Arc<dyn TerminalConfiguration + Send + Sync>,
        term_program: &str,
        term_version: &str,
        writer: Box<dyn std::io::Write + Send>,
    ) -> Terminal {
        Terminal {
            state: TerminalState::new_with_fixed_scrollback(
                plan.size,
                plan.configured_scrollback_rows,
                config,
                term_program,
                term_version,
                writer,
            ),
            parser: Parser::new(),
            bounded_terminal_identity: Some(Arc::new(())),
            server_retained: None,
            parser_retained_upper_bound: 0,
        }
    }

    /// Construct a production server terminal after its complete initial footprint is charged.
    pub fn new_server_from_geometry_plan(
        plan: TerminalConstructionGeometryPlan,
        lease: RetainedStateLease,
        geometry_limits: TerminalGeometryLimits,
        config: Arc<dyn TerminalConfiguration + Send + Sync>,
        term_program: &str,
        term_version: &str,
        writer: Box<dyn std::io::Write + Send>,
    ) -> Result<Terminal, TerminalRetainedStateError> {
        if lease.class() != RetainedClass::ServerTerminal {
            return Err(TerminalRetainedStateError::Admission(
                AdmissionError::InvalidFormula("server terminal requires ServerTerminal lease"),
            ));
        }
        let identity_bytes = term_program.len().checked_add(term_version.len()).ok_or(
            TerminalRetainedStateError::RetainedStateOverflow {
                calculation: "initial server terminal identity",
            },
        )?;
        if identity_bytes > MAX_SERVER_TERMINAL_IDENTITY_BYTES {
            return Err(TerminalRetainedStateError::InitialIdentityTooLarge {
                actual: identity_bytes,
                maximum: MAX_SERVER_TERMINAL_IDENTITY_BYTES,
            });
        }
        let initial = plan.initial_server_retained_bytes()?;
        if lease.bytes() < initial {
            return Err(TerminalRetainedStateError::RetainedStateInvariant {
                measured: initial,
                reserved: lease.bytes(),
            });
        }
        let mut terminal = Terminal {
            state: TerminalState::new_with_fixed_scrollback(
                plan.size,
                plan.configured_scrollback_rows,
                config,
                term_program,
                term_version,
                writer,
            ),
            parser: Parser::new(),
            bounded_terminal_identity: Some(Arc::new(())),
            server_retained: Some(ServerTerminalRetainedState {
                lease,
                geometry_limits,
            }),
            parser_retained_upper_bound: 0,
        };
        terminal.reconcile_server_retained_state()?;
        Ok(terminal)
    }

    pub fn server_retained_bytes(&self) -> Option<usize> {
        self.server_retained
            .as_ref()
            .map(|state| state.lease.bytes())
    }

    fn measured_server_retained_bytes(&self) -> Result<usize, TerminalRetainedStateError> {
        MAX_SERVER_TERMINAL_FIXED_BYTES
            .checked_add(self.state.retained_state_bytes_excluding_fixed()?)
            .and_then(|bytes| bytes.checked_add(self.parser_retained_upper_bound))
            .ok_or(TerminalRetainedStateError::RetainedStateOverflow {
                calculation: "measured server terminal retained state",
            })
    }

    fn action_batch_size(
        actions: &[Action],
        action_capacity: usize,
    ) -> Result<(usize, bool), TerminalRetainedStateError> {
        let mut retained_bytes = action_capacity
            .saturating_sub(actions.len())
            .saturating_mul(mem::size_of::<Action>());
        let mut contains_image = false;
        for action in actions {
            retained_bytes = retained_bytes.saturating_add(action.retained_size_upper_bound());
            contains_image |= matches!(action, Action::Sixel(_) | Action::KittyImage(_))
                || matches!(
                    action,
                    Action::OperatingSystemCommand(command)
                        if matches!(
                            command.as_ref(),
                            OperatingSystemCommand::ITermProprietary(
                                wezterm_escape_parser::osc::ITermProprietary::File(_)
                            )
                        )
                );
        }
        if retained_bytes > MAX_SERVER_TERMINAL_ACTION_BYTES {
            return Err(TerminalRetainedStateError::ActionBatchTooLarge {
                actual: retained_bytes,
                maximum: MAX_SERVER_TERMINAL_ACTION_BYTES,
            });
        }
        Ok((retained_bytes, contains_image))
    }

    fn action_peak_retained_bytes(
        &self,
        actions: &[Action],
        action_capacity: usize,
    ) -> Result<usize, TerminalRetainedStateError> {
        let current = self
            .server_retained
            .as_ref()
            .ok_or(TerminalRetainedStateError::Admission(
                AdmissionError::InvalidFormula("missing server terminal retained-state owner"),
            ))?
            .lease
            .bytes();
        let (action_bytes, contains_image) = Self::action_batch_size(actions, action_capacity)?;
        let action_growth = action_bytes
            .checked_mul(SERVER_TERMINAL_ACTION_AMPLIFICATION)
            .ok_or(TerminalRetainedStateError::RetainedStateOverflow {
                calculation: "terminal action retained growth",
            })?;
        let size = self.state.get_size();
        let visible_cells = size.rows.checked_mul(size.cols).ok_or(
            TerminalRetainedStateError::RetainedStateOverflow {
                calculation: "terminal visible cell count",
            },
        )?;
        let pen_bytes = self.state.pen_retained_heap_size_excluding_shared_data();
        let full_screen_attribute_growth = visible_cells.checked_mul(pen_bytes).ok_or(
            TerminalRetainedStateError::RetainedStateOverflow {
                calculation: "terminal full-screen attribute growth",
            },
        )?;
        let image_growth = if contains_image {
            MAX_SERVER_TERMINAL_IMAGE_MUTATION_BYTES
        } else {
            0
        };
        current
            .checked_add(action_growth)
            .and_then(|bytes| bytes.checked_add(full_screen_attribute_growth))
            .and_then(|bytes| bytes.checked_add(image_growth))
            .ok_or(TerminalRetainedStateError::RetainedStateOverflow {
                calculation: "terminal action peak retained state",
            })
    }

    fn reconcile_server_retained_state(&mut self) -> Result<(), TerminalRetainedStateError> {
        let measured = self.measured_server_retained_bytes()?;
        let retained =
            self.server_retained
                .as_mut()
                .ok_or(TerminalRetainedStateError::Admission(
                    AdmissionError::InvalidFormula("missing server terminal retained-state owner"),
                ))?;
        if measured > retained.lease.bytes() {
            return Err(TerminalRetainedStateError::RetainedStateInvariant {
                measured,
                reserved: retained.lease.bytes(),
            });
        }
        retained.lease.try_resize(measured)?;
        Ok(())
    }

    fn reserve_action_peak(
        &mut self,
        actions: &[Action],
        action_capacity: usize,
    ) -> Result<(), TerminalRetainedStateError> {
        let mut peak = self.action_peak_retained_bytes(actions, action_capacity)?;
        let first_result = self
            .server_retained
            .as_mut()
            .ok_or(TerminalRetainedStateError::Admission(
                AdmissionError::InvalidFormula("missing server terminal retained-state owner"),
            ))?
            .lease
            .try_resize(peak);
        if let Err(first_error) = first_result {
            self.state.evict_unreferenced_retained_state();
            self.reconcile_server_retained_state()?;
            peak = self.action_peak_retained_bytes(actions, action_capacity)?;
            self.server_retained
                .as_mut()
                .expect("server retained state disappeared")
                .lease
                .try_resize(peak)
                .map_err(|_| first_error)?;
        }
        Ok(())
    }

    /// Plan a resize for a fixed-scrollback terminal without mutating it.
    ///
    /// Geometry includes both screen line-slot buffers, retained line metadata, cloned
    /// graphemes and attributes excluding shared ImageData payloads, blank-line heaps, and tab
    /// storage. Parser state, titles, image payloads, caches, and maps are intentionally outside
    /// this geometry boundary.
    pub fn plan_bounded_resize(
        &self,
        target: TerminalSize,
        limits: TerminalGeometryLimits,
    ) -> Result<TerminalResizeGeometryPlan, TerminalGeometryError> {
        let terminal_identity = self
            .bounded_terminal_identity
            .as_ref()
            .map(Arc::clone)
            .ok_or(TerminalGeometryError::ResizeRequiresBoundedTerminal)?;
        let fixed_scrollback_rows = self
            .state
            .fixed_scrollback_rows()
            .ok_or(TerminalGeometryError::ResizeRequiresBoundedTerminal)?;

        Self::plan_bounded_construction(target, fixed_scrollback_rows, limits)?;
        let state_plan = self.state.plan_bounded_resize(target)?;
        let peak_geometry_bytes = state_plan.peak_geometry_bytes();
        if peak_geometry_bytes > limits.max_geometry_bytes {
            return Err(TerminalGeometryError::GeometryBytesExceedLimit {
                required: peak_geometry_bytes,
                limit: limits.max_geometry_bytes,
            });
        }
        let current_geometry_retained_bytes = state_plan.current_geometry_retained_bytes();
        let additional_bytes_required = peak_geometry_bytes
            .checked_sub(current_geometry_retained_bytes)
            .ok_or(TerminalGeometryError::ArithmeticOverflow {
                calculation: "bounded resize additional bytes",
            })?;

        Ok(TerminalResizeGeometryPlan {
            terminal_identity,
            epoch: self.state.current_seqno(),
            geometry_mutation_epoch: self.state.geometry_mutation_epoch(),
            target,
            current_geometry_retained_bytes,
            settled_geometry_retained_upper_bound: state_plan
                .settled_geometry_retained_upper_bound(),
            peak_geometry_bytes,
            additional_bytes_required,
            state_plan,
        })
    }

    pub fn prepare_server_resize(
        &mut self,
        target: TerminalSize,
    ) -> Result<PreparedServerTerminalResize<'_>, TerminalRetainedStateError> {
        let limits = self
            .server_retained
            .as_ref()
            .ok_or(TerminalRetainedStateError::Admission(
                AdmissionError::InvalidFormula("missing server terminal retained-state owner"),
            ))?
            .geometry_limits;
        let plan = self.plan_bounded_resize(target, limits)?;
        let current = self
            .server_retained
            .as_ref()
            .expect("validated above")
            .lease
            .bytes();
        let peak = current
            .checked_sub(plan.current_geometry_retained_bytes())
            .and_then(|bytes| bytes.checked_add(plan.peak_geometry_bytes()))
            .ok_or(TerminalRetainedStateError::RetainedStateOverflow {
                calculation: "server terminal resize peak",
            })?;
        let settled_retained_bytes = current
            .checked_sub(plan.current_geometry_retained_bytes())
            .and_then(|bytes| bytes.checked_add(plan.settled_geometry_retained_upper_bound()))
            .ok_or(TerminalRetainedStateError::RetainedStateOverflow {
                calculation: "server terminal settled resize state",
            })?;
        if settled_retained_bytes > peak {
            return Err(TerminalRetainedStateError::RetainedStateInvariant {
                measured: settled_retained_bytes,
                reserved: peak,
            });
        }
        let retained = self.server_retained.as_mut().expect("validated above");
        let prior_retained_bytes = retained.lease.bytes();
        retained.lease.try_resize(peak)?;
        Ok(PreparedServerTerminalResize {
            terminal: self,
            plan: Some(plan),
            prior_retained_bytes,
            settled_retained_bytes,
            committed: false,
        })
    }

    /// Consume and apply a non-stale bounded resize plan.
    pub fn apply_bounded_resize(
        &mut self,
        plan: TerminalResizeGeometryPlan,
    ) -> Result<(), TerminalGeometryError> {
        if !self
            .bounded_terminal_identity
            .as_ref()
            .map(|identity| Arc::ptr_eq(identity, &plan.terminal_identity))
            .unwrap_or(false)
        {
            return Err(TerminalGeometryError::ResizePlanTerminalMismatch);
        }
        let current_epoch = self.state.current_seqno();
        if current_epoch != plan.epoch {
            return Err(TerminalGeometryError::StaleResizePlan {
                planned_epoch: plan.epoch,
                current_epoch,
            });
        }
        let current_geometry_mutation_epoch = self.state.geometry_mutation_epoch();
        if current_geometry_mutation_epoch != plan.geometry_mutation_epoch {
            return Err(TerminalGeometryError::ResizePlanGeometryMutated {
                planned_epoch: plan.geometry_mutation_epoch,
                current_epoch: current_geometry_mutation_epoch,
            });
        }
        self.state
            .apply_bounded_resize(plan.target, plan.state_plan);
        Ok(())
    }

    /// Measure current retained terminal geometry, excluding shared ImageData payloads.
    pub fn geometry_retained_size_excluding_image_data(
        &self,
    ) -> Result<usize, TerminalGeometryError> {
        self.state.geometry_retained_size_excluding_image_data()
    }

    /// Feed the terminal parser a slice of bytes from the output
    /// of the associated program.
    /// The slice is not required to be a complete sequence of escape
    /// characters; it is valid to feed in chunks of data as they arrive.
    /// The output is parsed and applied to the terminal model.
    pub fn advance_bytes<B: AsRef<[u8]>>(
        &mut self,
        bytes: B,
    ) -> Result<(), TerminalRetainedStateError> {
        let bytes = bytes.as_ref();
        if self.server_retained.is_some() {
            let parser_growth = bytes
                .len()
                .checked_mul(SERVER_TERMINAL_ACTION_AMPLIFICATION)
                .ok_or(TerminalRetainedStateError::RetainedStateOverflow {
                    calculation: "terminal parser retained growth",
                })?;
            let image_parser_growth = if bytes.is_empty() {
                0
            } else {
                MAX_SERVER_TERMINAL_IMAGE_MUTATION_BYTES
            };
            let peak = self
                .server_retained
                .as_ref()
                .expect("validated above")
                .lease
                .bytes()
                .checked_add(parser_growth)
                .and_then(|retained| retained.checked_add(image_parser_growth))
                .ok_or(TerminalRetainedStateError::RetainedStateOverflow {
                    calculation: "terminal parser retained peak",
                })?;
            self.server_retained
                .as_mut()
                .expect("validated above")
                .lease
                .try_resize(peak)?;
        }
        let actions = self.parser.parse_as_vec(bytes);
        if self.server_retained.is_some() {
            self.parser_retained_upper_bound = self.parser.retained_size_upper_bound();
            self.reconcile_server_retained_state()?;
        }
        self.perform_actions(actions)?;
        Ok(())
    }

    pub fn perform_actions(
        &mut self,
        actions: Vec<wezterm_escape_parser::Action>,
    ) -> Result<(), TerminalRetainedStateError> {
        if self.server_retained.is_some() {
            self.reserve_action_peak(&actions, actions.capacity())?;
        }
        self.state.increment_seqno();
        {
            let mut performer = Performer::new(&mut self.state);
            for action in actions {
                performer.perform(action);
            }
        }
        self.trigger_unseen_output_notif();
        if self.server_retained.is_some() {
            self.reconcile_server_retained_state()?;
        }
        Ok(())
    }
}
