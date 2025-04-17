use super::select_tool::extend_lasso;
use super::tool_prelude::*;
use crate::consts::{
	COLOR_OVERLAY_BLUE, DRAG_DIRECTION_MODE_DETERMINATION_THRESHOLD, DRAG_THRESHOLD, HANDLE_ROTATE_SNAP_ANGLE, INSERT_POINT_ON_SEGMENT_TOO_FAR_DISTANCE, SEGMENT_INSERTION_TOLERANCE,
	SEGMENT_OVERLAY_SIZE, SELECTION_THRESHOLD, SELECTION_TOLERANCE,
};
use crate::messages::portfolio::document::overlays::utility_functions::{path_overlays, selected_segments};
use crate::messages::portfolio::document::overlays::utility_types::{DrawHandles, OverlayContext};
use crate::messages::portfolio::document::utility_types::document_metadata::LayerNodeIdentifier;
use crate::messages::portfolio::document::utility_types::network_interface::NodeNetworkInterface;
use crate::messages::portfolio::document::utility_types::transformation::Axis;
use crate::messages::preferences::SelectionMode;
use crate::messages::tool::common_functionality::auto_panning::AutoPanning;
use crate::messages::tool::common_functionality::shape_editor::{
	ClosestSegment, ManipulatorAngle, OpposingHandleLengths, SelectedPointsInfo, SelectionChange, SelectionShape, SelectionShapeType, ShapeState,
};
use crate::messages::tool::common_functionality::snapping::{SnapCache, SnapCandidatePoint, SnapConstraint, SnapData, SnapManager};
use graphene_core::renderer::Quad;
use graphene_core::vector::{ManipulatorPointId, PointId, VectorModificationType};
use graphene_std::vector::{HandleId, NoHashBuilder, SegmentId, VectorData};
use std::vec;

#[derive(Default)]
pub struct PathTool {
	fsm_state: PathToolFsmState,
	tool_data: PathToolData,
	options: PathToolOptions,
}

#[derive(Default)]
pub struct PathToolOptions {
	path_overlay_mode: PathOverlayMode,
}

#[impl_message(Message, ToolMessage, Path)]
#[derive(PartialEq, Clone, Debug, serde::Serialize, serde::Deserialize, specta::Type)]
pub enum PathToolMessage {
	// Standard messages
	Abort,
	Overlays(OverlayContext),
	SelectionChanged,

	// Tool-specific messages
	BreakPath,
	DeselectAllPoints,
	Delete,
	DeleteAndBreakPath,
	DragStop {
		extend_selection: Key,
		shrink_selection: Key,
	},
	Enter {
		extend_selection: Key,
		shrink_selection: Key,
	},
	Escape,
	ClosePath,
	FlipSmoothSharp,
	GRS {
		// Should be `Key::KeyG` (Grab), `Key::KeyR` (Rotate), or `Key::KeyS` (Scale)
		key: Key,
	},
	ManipulatorMakeHandlesFree,
	ManipulatorMakeHandlesColinear,
	MouseDown {
		delete_segment: Key,
		extend_selection: Key,
		lasso_select: Key,
		handle_drag_from_anchor: Key,
	},
	NudgeSelectedPoints {
		delta_x: f64,
		delta_y: f64,
	},
	PointerMove {
		equidistant: Key,
		toggle_colinear: Key,
		move_anchor_with_handles: Key,
		snap_angle: Key,
		lock_angle: Key,
	},
	PointerOutsideViewport {
		equidistant: Key,
		toggle_colinear: Key,
		move_anchor_with_handles: Key,
		snap_angle: Key,
		lock_angle: Key,
	},
	RightClick,
	SelectAllAnchors,
	SelectedPointUpdated,
	SelectedPointXChanged {
		new_x: f64,
	},
	SelectedPointYChanged {
		new_y: f64,
	},
	SwapSelectedHandles,
	UpdateOptions(PathOptionsUpdate),
}

#[derive(PartialEq, Eq, Hash, Copy, Clone, Debug, Default, serde::Serialize, serde::Deserialize, specta::Type)]
pub enum PathOverlayMode {
	AllHandles = 0,
	#[default]
	SelectedPointHandles = 1,
	FrontierHandles = 2,
}

#[derive(PartialEq, Eq, Clone, Debug, Hash, serde::Serialize, serde::Deserialize, specta::Type)]
pub enum PathOptionsUpdate {
	OverlayModeType(PathOverlayMode),
}

impl ToolMetadata for PathTool {
	fn icon_name(&self) -> String {
		"VectorPathTool".into()
	}
	fn tooltip(&self) -> String {
		"Path Tool".into()
	}
	fn tool_type(&self) -> crate::messages::tool::utility_types::ToolType {
		ToolType::Path
	}
}

impl LayoutHolder for PathTool {
	fn layout(&self) -> Layout {
		let coordinates = self.tool_data.selection_status.as_one().as_ref().map(|point| point.coordinates);
		let (x, y) = coordinates.map(|point| (Some(point.x), Some(point.y))).unwrap_or((None, None));

		let selection_status = &self.tool_data.selection_status;
		let manipulator_angle = selection_status.angle();

		let x_location = NumberInput::new(x)
			.unit(" px")
			.label("X")
			.min_width(120)
			.disabled(x.is_none())
			.min(-((1_u64 << f64::MANTISSA_DIGITS) as f64))
			.max((1_u64 << f64::MANTISSA_DIGITS) as f64)
			.on_update(move |number_input: &NumberInput| {
				if let Some(new_x) = number_input.value.or(x) {
					PathToolMessage::SelectedPointXChanged { new_x }.into()
				} else {
					Message::NoOp
				}
			})
			.widget_holder();

		let y_location = NumberInput::new(y)
			.unit(" px")
			.label("Y")
			.min_width(120)
			.disabled(y.is_none())
			.min(-((1_u64 << f64::MANTISSA_DIGITS) as f64))
			.max((1_u64 << f64::MANTISSA_DIGITS) as f64)
			.on_update(move |number_input: &NumberInput| {
				if let Some(new_y) = number_input.value.or(y) {
					PathToolMessage::SelectedPointYChanged { new_y }.into()
				} else {
					Message::NoOp
				}
			})
			.widget_holder();

		let related_seperator = Separator::new(SeparatorType::Related).widget_holder();
		let unrelated_seperator = Separator::new(SeparatorType::Unrelated).widget_holder();

		let colinear_handles_tooltip = "Keep both handles unbent, each 180° apart, when moving either";
		let colinear_handles_state = manipulator_angle.and_then(|angle| match angle {
			ManipulatorAngle::Colinear => Some(true),
			ManipulatorAngle::Free => Some(false),
			ManipulatorAngle::Mixed => None,
		})
		// TODO: Remove `unwrap_or_default` once checkboxes are capable of displaying a mixed state
		.unwrap_or_default();
		let colinear_handle_checkbox = CheckboxInput::new(colinear_handles_state)
			.disabled(!self.tool_data.can_toggle_colinearity)
			.on_update(|&CheckboxInput { checked, .. }| {
				if checked {
					PathToolMessage::ManipulatorMakeHandlesColinear.into()
				} else {
					PathToolMessage::ManipulatorMakeHandlesFree.into()
				}
			})
			.tooltip(colinear_handles_tooltip)
			.widget_holder();
		let colinear_handles_label = TextLabel::new("Colinear Handles")
			.disabled(!self.tool_data.can_toggle_colinearity)
			.tooltip(colinear_handles_tooltip)
			.widget_holder();

		let path_overlay_mode_widget = RadioInput::new(vec![
			RadioEntryData::new("all")
				.icon("HandleVisibilityAll")
				.tooltip("Show all handles regardless of selection")
				.on_update(move |_| PathToolMessage::UpdateOptions(PathOptionsUpdate::OverlayModeType(PathOverlayMode::AllHandles)).into()),
			RadioEntryData::new("selected")
				.icon("HandleVisibilitySelected")
				.tooltip("Show only handles of the segments connected to selected points")
				.on_update(move |_| PathToolMessage::UpdateOptions(PathOptionsUpdate::OverlayModeType(PathOverlayMode::SelectedPointHandles)).into()),
			RadioEntryData::new("frontier")
				.icon("HandleVisibilityFrontier")
				.tooltip("Show only handles at the frontiers of the segments connected to selected points")
				.on_update(move |_| PathToolMessage::UpdateOptions(PathOptionsUpdate::OverlayModeType(PathOverlayMode::FrontierHandles)).into()),
		])
		.selected_index(Some(self.options.path_overlay_mode as u32))
		.widget_holder();

		Layout::WidgetLayout(WidgetLayout::new(vec![LayoutGroup::Row {
			widgets: vec![
				x_location,
				related_seperator.clone(),
				y_location,
				unrelated_seperator.clone(),
				colinear_handle_checkbox,
				related_seperator,
				colinear_handles_label,
				unrelated_seperator,
				path_overlay_mode_widget,
			],
		}]))
	}
}

impl<'a> MessageHandler<ToolMessage, &mut ToolActionHandlerData<'a>> for PathTool {
	fn process_message(&mut self, message: ToolMessage, responses: &mut VecDeque<Message>, tool_data: &mut ToolActionHandlerData<'a>) {
		let updating_point = message == ToolMessage::Path(PathToolMessage::SelectedPointUpdated);

		match message {
			ToolMessage::Path(PathToolMessage::UpdateOptions(action)) => match action {
				PathOptionsUpdate::OverlayModeType(overlay_mode_type) => {
					self.options.path_overlay_mode = overlay_mode_type;
					responses.add(OverlaysMessage::Draw);
				}
			},
			ToolMessage::Path(PathToolMessage::ClosePath) => {
				responses.add(DocumentMessage::AddTransaction);
				tool_data.shape_editor.close_selected_path(tool_data.document, responses);
				responses.add(DocumentMessage::EndTransaction);
				responses.add(OverlaysMessage::Draw);
			}
			ToolMessage::Path(PathToolMessage::SwapSelectedHandles) => {
				if tool_data.shape_editor.handle_with_pair_selected(&tool_data.document.network_interface) {
					tool_data.shape_editor.alternate_selected_handles(&tool_data.document.network_interface);
					responses.add(PathToolMessage::SelectedPointUpdated);
					responses.add(FrontendMessage::UpdateMouseCursor { cursor: MouseCursorIcon::None });
					responses.add(OverlaysMessage::Draw);
				}
			}
			_ => {
				self.fsm_state.process_event(message, &mut self.tool_data, tool_data, &self.options, responses, true);
			}
		}

		if updating_point {
			self.send_layout(responses, LayoutTarget::ToolOptions);
		}
	}

	// Different actions depending on state may be wanted:
	fn actions(&self) -> ActionList {
		match self.fsm_state {
			PathToolFsmState::Ready => actions!(PathToolMessageDiscriminant;
				FlipSmoothSharp,
				MouseDown,
				Delete,
				NudgeSelectedPoints,
				Enter,
				SelectAllAnchors,
				DeselectAllPoints,
				BreakPath,
				DeleteAndBreakPath,
				ClosePath,
				PointerMove,
			),
			PathToolFsmState::Dragging(_) => actions!(PathToolMessageDiscriminant;
				Escape,
				RightClick,
				FlipSmoothSharp,
				DragStop,
				PointerMove,
				Delete,
				BreakPath,
				DeleteAndBreakPath,
				SwapSelectedHandles,
			),
			PathToolFsmState::Drawing { .. } => actions!(PathToolMessageDiscriminant;
				FlipSmoothSharp,
				DragStop,
				PointerMove,
				Delete,
				Enter,
				BreakPath,
				DeleteAndBreakPath,
				Escape,
				RightClick,
			),
			PathToolFsmState::InsertPoint => actions!(PathToolMessageDiscriminant;
				Enter,
				MouseDown,
				PointerMove,
				Escape,
				Delete,
				RightClick,
				GRS,
			),
		}
	}
}

impl ToolTransition for PathTool {
	fn event_to_message_map(&self) -> EventToMessageMap {
		EventToMessageMap {
			tool_abort: Some(PathToolMessage::Abort.into()),
			selection_changed: Some(PathToolMessage::SelectionChanged.into()),
			overlay_provider: Some(|overlay_context| PathToolMessage::Overlays(overlay_context).into()),
			..Default::default()
		}
	}
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DraggingState {
	point_select_state: PointSelectState,
	colinear: ManipulatorAngle,
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub enum PointSelectState {
	HandleWithPair,
	#[default]
	HandleNoPair,
	Anchor,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum PathToolFsmState {
	#[default]
	Ready,
	Dragging(DraggingState),
	Drawing {
		selection_shape: SelectionShapeType,
	},
	InsertPoint,
}

enum InsertEndKind {
	Abort,
	Add { extend_selection: bool },
}

#[derive(Default)]
struct PathToolData {
	snap_manager: SnapManager,
	lasso_polygon: Vec<DVec2>,
	selection_mode: Option<SelectionMode>,
	drag_start_pos: DVec2,
	previous_mouse_position: DVec2,
	toggle_colinear_debounce: bool,
	opposing_handle_lengths: Option<OpposingHandleLengths>,
	/// Describes information about the selected point(s), if any, across one or multiple shapes and manipulator point types (anchor or handle).
	/// The available information varies depending on whether `None`, `One`, or `Multiple` points are currently selected.
	/// NOTE: It must be updated using `update_selection_status` to ensure `can_toggle_colinearity` stays synchronized with the current selection.
	selection_status: SelectionStatus,
	/// `true` if we can change the current selection to colinear or not.
	can_toggle_colinearity: bool,
	segment: Option<ClosestSegment>,
	snap_cache: SnapCache,
	double_click_handled: bool,
	delete_segment_pressed: bool,
	auto_panning: AutoPanning,
	saved_points_before_anchor_select_toggle: Vec<ManipulatorPointId>,
	select_anchor_toggled: bool,
	saved_points_before_handle_drag: Vec<ManipulatorPointId>,
	handle_drag_toggle: bool,
	dragging_state: DraggingState,
	current_selected_handle_id: Option<ManipulatorPointId>,
	angle: f64,
	opposite_handle_position: Option<DVec2>,
	last_clicked_point_was_selected: bool,
	snapping_axis: Option<Axis>,
	alt_clicked_on_anchor: bool,
	alt_dragging_from_anchor: bool,
}

impl PathToolData {
	fn save_points_before_anchor_toggle(&mut self, points: Vec<ManipulatorPointId>) -> PathToolFsmState {
		self.saved_points_before_anchor_select_toggle = points;
		PathToolFsmState::Dragging(self.dragging_state)
	}

	fn remove_saved_points(&mut self) {
		self.saved_points_before_anchor_select_toggle.clear();
	}

	pub fn selection_quad(&self) -> Quad {
		let bbox = self.selection_box();
		Quad::from_box(bbox)
	}

	pub fn calculate_selection_mode_from_direction(&mut self) -> SelectionMode {
		let bbox = self.selection_box();
		let above_threshold = bbox[1].distance_squared(bbox[0]) > DRAG_DIRECTION_MODE_DETERMINATION_THRESHOLD.powi(2);

		if self.selection_mode.is_none() && above_threshold {
			let mode = if bbox[1].x < bbox[0].x {
				SelectionMode::Touched
			} else {
				// This also covers the case where they're equal: the area is zero, so we use `Enclosed` to ensure the selection ends up empty, as nothing will be enclosed by an empty area
				SelectionMode::Enclosed
			};
			self.selection_mode = Some(mode);
		}

		self.selection_mode.unwrap_or(SelectionMode::Touched)
	}

	pub fn selection_box(&self) -> [DVec2; 2] {
		if self.previous_mouse_position == self.drag_start_pos {
			let tolerance = DVec2::splat(SELECTION_TOLERANCE);
			[self.drag_start_pos - tolerance, self.drag_start_pos + tolerance]
		} else {
			[self.drag_start_pos, self.previous_mouse_position]
		}
	}

	fn update_selection_status(&mut self, shape_editor: &mut ShapeState, document: &DocumentMessageHandler) {
		let selection_status = get_selection_status(&document.network_interface, shape_editor);

		self.can_toggle_colinearity = match &selection_status {
			SelectionStatus::None => false,
			SelectionStatus::One(single_selected_point) => {
				let vector_data = document.network_interface.compute_modified_vector(single_selected_point.layer).unwrap();
				single_selected_point.id.get_handle_pair(&vector_data).is_some()
			}
			SelectionStatus::Multiple(_) => true,
		};
		self.selection_status = selection_status;
	}

	fn start_insertion(&mut self, responses: &mut VecDeque<Message>, segment: ClosestSegment) -> PathToolFsmState {
		if self.segment.is_some() {
			warn!("Segment was `Some(..)` before `start_insertion`")
		}
		self.segment = Some(segment);
		responses.add(OverlaysMessage::Draw);
		PathToolFsmState::InsertPoint
	}

	fn update_insertion(&mut self, shape_editor: &mut ShapeState, document: &DocumentMessageHandler, responses: &mut VecDeque<Message>, input: &InputPreprocessorMessageHandler) -> PathToolFsmState {
		if let Some(closed_segment) = &mut self.segment {
			closed_segment.update_closest_point(document.metadata(), input.mouse.position);
			if closed_segment.too_far(input.mouse.position, INSERT_POINT_ON_SEGMENT_TOO_FAR_DISTANCE, document.metadata()) {
				self.end_insertion(shape_editor, responses, InsertEndKind::Abort)
			} else {
				PathToolFsmState::InsertPoint
			}
		} else {
			warn!("Segment was `None` on `update_insertion`");
			PathToolFsmState::Ready
		}
	}

	fn end_insertion(&mut self, shape_editor: &mut ShapeState, responses: &mut VecDeque<Message>, kind: InsertEndKind) -> PathToolFsmState {
		let mut commit_transaction = false;
		self.delete_segment_pressed = false;
		match self.segment.as_mut() {
			None => {
				warn!("Segment was `None` before `end_insertion`")
			}
			Some(closed_segment) => {
				if let InsertEndKind::Add { extend_selection } = kind {
					closed_segment.adjusted_insert_and_select(shape_editor, responses, extend_selection);
					commit_transaction = true;
				}
			}
		}

		self.segment = None;
		if commit_transaction {
			responses.add(DocumentMessage::EndTransaction);
		} else {
			responses.add(DocumentMessage::AbortTransaction);
		}
		responses.add(OverlaysMessage::Draw);
		PathToolFsmState::Ready
	}

	#[allow(clippy::too_many_arguments)]
	fn mouse_down(
		&mut self,
		shape_editor: &mut ShapeState,
		document: &DocumentMessageHandler,
		input: &InputPreprocessorMessageHandler,
		responses: &mut VecDeque<Message>,
		extend_selection: bool,
		lasso_select: bool,
		handle_drag_from_anchor: bool,
	) -> PathToolFsmState {
		self.double_click_handled = false;
		self.opposing_handle_lengths = None;

		self.drag_start_pos = input.mouse.position;

		let old_selection = shape_editor.selected_points().cloned().collect::<Vec<_>>();

		// Check if the point is already selected; if not, select the first point within the threshold (in pixels)
		if let Some((already_selected, mut selection_info)) = shape_editor.get_point_selection_state(&document.network_interface, input.mouse.position, SELECTION_THRESHOLD) {
			responses.add(DocumentMessage::StartTransaction);

			self.last_clicked_point_was_selected = already_selected;

			// If the point is already selected and shift (`extend_selection`) is used, keep the selection unchanged.
			// Otherwise, select the first point within the threshold.
			if !(already_selected && extend_selection) {
				if let Some(updated_selection_info) = shape_editor.change_point_selection(&document.network_interface, input.mouse.position, SELECTION_THRESHOLD, extend_selection) {
					selection_info = updated_selection_info;
				}
			}

			if let Some(selected_points) = selection_info {
				self.drag_start_pos = input.mouse.position;

				// If selected points contain only handles and there was some selection before, then it is stored and becomes restored upon release
				let mut dragging_only_handles = true;
				for point in &selected_points.points {
					if matches!(point.point_id, ManipulatorPointId::Anchor(_)) {
						dragging_only_handles = false;
						break;
					}
				}
				if dragging_only_handles && !self.handle_drag_toggle && !old_selection.is_empty() {
					self.saved_points_before_handle_drag = old_selection;
				}

				if handle_drag_from_anchor {
					if let Some((layer, point)) = shape_editor.find_nearest_point_indices(&document.network_interface, input.mouse.position, SELECTION_THRESHOLD) {
						// Check that selected point is an anchor
						if let (Some(point_id), Some(vector_data)) = (point.as_anchor(), document.network_interface.compute_modified_vector(layer)) {
							let handles = vector_data.all_connected(point_id).collect::<Vec<_>>();
							self.alt_clicked_on_anchor = true;
							for handle in &handles {
								let modification_type = handle.set_relative_position(DVec2::ZERO);
								responses.add(GraphOperationMessage::Vector { layer, modification_type });
								for &handles in &vector_data.colinear_manipulators {
									if handles.contains(&handle) {
										let modification_type = VectorModificationType::SetG1Continuous { handles, enabled: false };
										responses.add(GraphOperationMessage::Vector { layer, modification_type });
									}
								}
							}

							let manipulator_point_id = handles[0].to_manipulator_point();
							shape_editor.deselect_all_points();
							shape_editor.select_points_by_manipulator_id(&vec![manipulator_point_id]);
							responses.add(PathToolMessage::SelectedPointUpdated);
						}
					}
				}

				self.start_dragging_point(selected_points, input, document, shape_editor);
				responses.add(OverlaysMessage::Draw);
			}
			PathToolFsmState::Dragging(self.dragging_state)
		}
		// We didn't find a point, so consider selecting the nearest shape instead
		else if let Some(layer) = document.click(input) {
			shape_editor.deselect_all_points();
			if extend_selection {
				responses.add(NodeGraphMessage::SelectedNodesAdd { nodes: vec![layer.to_node()] });
			} else {
				responses.add(NodeGraphMessage::SelectedNodesSet { nodes: vec![layer.to_node()] });
			}
			self.drag_start_pos = input.mouse.position;
			self.previous_mouse_position = document.metadata().document_to_viewport.inverse().transform_point2(input.mouse.position);

			responses.add(DocumentMessage::StartTransaction);

			PathToolFsmState::Dragging(self.dragging_state)
		}
		// Start drawing
		else {
			self.drag_start_pos = input.mouse.position;
			self.previous_mouse_position = document.metadata().document_to_viewport.inverse().transform_point2(input.mouse.position);

			let selection_shape = if lasso_select { SelectionShapeType::Lasso } else { SelectionShapeType::Box };
			PathToolFsmState::Drawing { selection_shape }
		}
	}

	fn start_dragging_point(&mut self, selected_points: SelectedPointsInfo, input: &InputPreprocessorMessageHandler, document: &DocumentMessageHandler, shape_editor: &mut ShapeState) {
		let mut manipulators = HashMap::with_hasher(NoHashBuilder);
		let mut unselected = Vec::new();
		for (&layer, state) in &shape_editor.selected_shape_state {
			let Some(vector_data) = document.network_interface.compute_modified_vector(layer) else {
				continue;
			};
			let transform = document.metadata().transform_to_document(layer);

			let mut layer_manipulators = HashSet::with_hasher(NoHashBuilder);
			for point in state.selected() {
				let Some(anchor) = point.get_anchor(&vector_data) else { continue };
				layer_manipulators.insert(anchor);
				let Some([handle1, handle2]) = point.get_handle_pair(&vector_data) else { continue };
				let Some(handle) = point.as_handle() else { continue };
				// Check which handle is selected and which is opposite
				let opposite = if handle == handle1 { handle2 } else { handle1 };

				self.opposite_handle_position = if self.opposite_handle_position.is_none() {
					opposite.to_manipulator_point().get_position(&vector_data)
				} else {
					self.opposite_handle_position
				};
			}
			for (&id, &position) in vector_data.point_domain.ids().iter().zip(vector_data.point_domain.positions()) {
				if layer_manipulators.contains(&id) {
					continue;
				}
				unselected.push(SnapCandidatePoint::handle(transform.transform_point2(position)))
			}
			if !layer_manipulators.is_empty() {
				manipulators.insert(layer, layer_manipulators);
			}
		}
		self.snap_cache = SnapCache { manipulators, unselected };

		let viewport_to_document = document.metadata().document_to_viewport.inverse();
		self.previous_mouse_position = viewport_to_document.transform_point2(input.mouse.position - selected_points.offset);
	}

	fn update_colinear(&mut self, equidistant: bool, toggle_colinear: bool, shape_editor: &mut ShapeState, document: &DocumentMessageHandler, responses: &mut VecDeque<Message>) -> bool {
		// Check handle colinear state
		let is_colinear = self
			.selection_status
			.angle()
			.map(|angle| match angle {
				ManipulatorAngle::Colinear => true,
				ManipulatorAngle::Free | ManipulatorAngle::Mixed => false,
			})
			.unwrap_or(false);

		// Check if the toggle_colinear key has just been pressed
		if toggle_colinear && !self.toggle_colinear_debounce {
			self.opposing_handle_lengths = None;
			if is_colinear {
				shape_editor.disable_colinear_handles_state_on_selected(&document.network_interface, responses);
			} else {
				shape_editor.convert_selected_manipulators_to_colinear_handles(responses, document);
			}
			self.toggle_colinear_debounce = true;
			return true;
		}
		self.toggle_colinear_debounce = toggle_colinear;

		if equidistant && self.opposing_handle_lengths.is_none() {
			if !is_colinear {
				// Try to get selected handle info
				let Some((_, _, selected_handle_id)) = self.try_get_selected_handle_and_anchor(shape_editor, document) else {
					self.opposing_handle_lengths = Some(shape_editor.opposing_handle_lengths(document));
					return false;
				};

				let Some((layer, _)) = shape_editor.selected_shape_state.iter().next() else {
					self.opposing_handle_lengths = Some(shape_editor.opposing_handle_lengths(document));
					return false;
				};

				let Some(vector_data) = document.network_interface.compute_modified_vector(*layer) else {
					self.opposing_handle_lengths = Some(shape_editor.opposing_handle_lengths(document));
					return false;
				};

				// Check if handle has a pair (to ignore handles of edges of open paths)
				if let Some(handle_pair) = selected_handle_id.get_handle_pair(&vector_data) {
					let opposite_handle_length = handle_pair.iter().filter(|&&h| h.to_manipulator_point() != selected_handle_id).find_map(|&h| {
						let opp_handle_pos = h.to_manipulator_point().get_position(&vector_data)?;
						let opp_anchor_id = h.to_manipulator_point().get_anchor(&vector_data)?;
						let opp_anchor_pos = vector_data.point_domain.position_from_id(opp_anchor_id)?;
						Some((opp_handle_pos - opp_anchor_pos).length())
					});

					// Make handles colinear if opposite handle is zero length
					if opposite_handle_length == Some(0.) {
						shape_editor.convert_selected_manipulators_to_colinear_handles(responses, document);
						return true;
					}
				}
			}
			self.opposing_handle_lengths = Some(shape_editor.opposing_handle_lengths(document));
		}
		false
	}

	/// Attempts to get a single selected handle. Also retrieves the position of the anchor it is connected to. Used for the purpose of snapping the angle.
	fn try_get_selected_handle_and_anchor(&self, shape_editor: &ShapeState, document: &DocumentMessageHandler) -> Option<(DVec2, DVec2, ManipulatorPointId)> {
		// Only count selections of a single layer
		let (layer, selection) = shape_editor.selected_shape_state.iter().next()?;

		// Do not allow selections of multiple points to count
		if selection.selected_points_count() != 1 {
			return None;
		}

		// Only count selected handles
		let selected_handle = selection.selected().next()?.as_handle()?;
		let handle_id = selected_handle.to_manipulator_point();

		let layer_to_document = document.metadata().transform_to_document(*layer);
		let vector_data = document.network_interface.compute_modified_vector(*layer)?;

		let handle_position_local = selected_handle.to_manipulator_point().get_position(&vector_data)?;
		let anchor_id = selected_handle.to_manipulator_point().get_anchor(&vector_data)?;
		let anchor_position_local = vector_data.point_domain.position_from_id(anchor_id)?;

		let handle_position_document = layer_to_document.transform_point2(handle_position_local);
		let anchor_position_document = layer_to_document.transform_point2(anchor_position_local);

		Some((handle_position_document, anchor_position_document, handle_id))
	}

	fn calculate_handle_angle(&mut self, handle_vector: DVec2, handle_id: ManipulatorPointId, lock_angle: bool, snap_angle: bool) -> f64 {
		let current_angle = -handle_vector.angle_to(DVec2::X);

		// When the angle is locked we use the old angle
		if self.current_selected_handle_id == Some(handle_id) && lock_angle {
			return self.angle;
		}

		// Round the angle to the closest increment
		let mut handle_angle = current_angle;
		if snap_angle && !lock_angle {
			let snap_resolution = HANDLE_ROTATE_SNAP_ANGLE.to_radians();
			handle_angle = (handle_angle / snap_resolution).round() * snap_resolution;
		}

		// Cache the angle and handle id for lock angle
		self.current_selected_handle_id = Some(handle_id);
		self.angle = handle_angle;

		handle_angle
	}

	#[allow(clippy::too_many_arguments)]
	fn apply_snapping(
		&mut self,
		handle_direction: DVec2,
		new_handle_position: DVec2,
		anchor_position: DVec2,
		using_angle_constraints: bool,
		handle_position: DVec2,
		document: &DocumentMessageHandler,
		input: &InputPreprocessorMessageHandler,
	) -> DVec2 {
		let snap_data = SnapData::new(document, input);
		let snap_point = SnapCandidatePoint::handle_neighbors(new_handle_position, [anchor_position]);

		let snap_result = match using_angle_constraints {
			true => {
				let snap_constraint = SnapConstraint::Line {
					origin: anchor_position,
					direction: handle_direction.normalize_or_zero(),
				};
				self.snap_manager.constrained_snap(&snap_data, &snap_point, snap_constraint, Default::default())
			}
			false => self.snap_manager.free_snap(&snap_data, &snap_point, Default::default()),
		};

		self.snap_manager.update_indicator(snap_result.clone());

		document.metadata().document_to_viewport.transform_vector2(snap_result.snapped_point_document - handle_position)
	}

	fn start_snap_along_axis(&mut self, shape_editor: &mut ShapeState, document: &DocumentMessageHandler, input: &InputPreprocessorMessageHandler, responses: &mut VecDeque<Message>) {
		// Find the negative delta to take the point to the drag start position
		let current_mouse = input.mouse.position;
		let drag_start = self.drag_start_pos;
		let opposite_delta = drag_start - current_mouse;

		shape_editor.move_selected_points(None, document, opposite_delta, false, true, false, None, responses);

		// Calculate the projected delta and shift the points along that delta
		let delta = current_mouse - drag_start;
		let axis = if delta.x.abs() >= delta.y.abs() { Axis::X } else { Axis::Y };
		self.snapping_axis = Some(axis);
		let projected_delta = match axis {
			Axis::X => DVec2::new(delta.x, 0.),
			Axis::Y => DVec2::new(0., delta.y),
			_ => DVec2::new(delta.x, 0.),
		};

		shape_editor.move_selected_points(None, document, projected_delta, false, true, false, None, responses);
	}

	fn stop_snap_along_axis(&mut self, shape_editor: &mut ShapeState, document: &DocumentMessageHandler, input: &InputPreprocessorMessageHandler, responses: &mut VecDeque<Message>) {
		// Calculate the negative delta of the selection and move it back to the drag start
		let current_mouse = input.mouse.position;
		let drag_start = self.drag_start_pos;

		let opposite_delta = drag_start - current_mouse;
		let Some(axis) = self.snapping_axis else { return };
		let opposite_projected_delta = match axis {
			Axis::X => DVec2::new(opposite_delta.x, 0.),
			Axis::Y => DVec2::new(0., opposite_delta.y),
			_ => DVec2::new(opposite_delta.x, 0.),
		};

		shape_editor.move_selected_points(None, document, opposite_projected_delta, false, true, false, None, responses);

		// Calculate what actually would have been the original delta for the point, and apply that
		let delta = current_mouse - drag_start;

		shape_editor.move_selected_points(None, document, delta, false, true, false, None, responses);

		self.snapping_axis = None;
	}

	fn get_normalized_tangent(&mut self, point: PointId, segment: SegmentId, vector_data: &VectorData) -> Option<DVec2> {
		let other_point = vector_data.other_point(segment, point)?;
		let position = ManipulatorPointId::Anchor(point).get_position(vector_data)?;

		let mut handles = vector_data.all_connected(other_point);
		let other_handle = handles.find(|handle| handle.segment == segment)?;

		let target_position = if other_handle.length(vector_data) == 0. {
			ManipulatorPointId::Anchor(other_point).get_position(vector_data)?
		} else {
			other_handle.to_manipulator_point().get_position(vector_data)?
		};

		let tangent_vector = target_position - position;
		tangent_vector.try_normalize()
	}

	#[allow(clippy::too_many_arguments)]
	fn drag(
		&mut self,
		equidistant: bool,
		lock_angle: bool,
		snap_angle: bool,
		shape_editor: &mut ShapeState,
		document: &DocumentMessageHandler,
		input: &InputPreprocessorMessageHandler,
		responses: &mut VecDeque<Message>,
	) {
		// First check if selection is not just a single handle point
		let selected_points = shape_editor.selected_points();
		let single_handle_selected = selected_points.count() == 1
			&& shape_editor
				.selected_points()
				.any(|point| matches!(point, ManipulatorPointId::EndHandle(_) | ManipulatorPointId::PrimaryHandle(_)));

		if snap_angle && self.snapping_axis.is_none() && !single_handle_selected {
			self.start_snap_along_axis(shape_editor, document, input, responses);
		} else if !snap_angle && self.snapping_axis.is_some() {
			self.stop_snap_along_axis(shape_editor, document, input, responses);
		}

		let document_to_viewport = document.metadata().document_to_viewport;
		let previous_mouse = document_to_viewport.transform_point2(self.previous_mouse_position);
		let current_mouse = input.mouse.position;
		let raw_delta = document_to_viewport.inverse().transform_vector2(current_mouse - previous_mouse);

		let snapped_delta = if let Some((handle_pos, anchor_pos, handle_id)) = self.try_get_selected_handle_and_anchor(shape_editor, document) {
			let cursor_pos = handle_pos + raw_delta;

			let handle_angle = self.calculate_handle_angle(cursor_pos - anchor_pos, handle_id, lock_angle, snap_angle);

			let constrained_direction = DVec2::new(handle_angle.cos(), handle_angle.sin());
			let projected_length = (cursor_pos - anchor_pos).dot(constrained_direction);
			let constrained_target = anchor_pos + constrained_direction * projected_length;
			let constrained_delta = constrained_target - handle_pos;

			self.apply_snapping(constrained_direction, handle_pos + constrained_delta, anchor_pos, lock_angle || snap_angle, handle_pos, document, input)
		} else {
			shape_editor.snap(&mut self.snap_manager, &self.snap_cache, document, input, previous_mouse)
		};

		let handle_lengths = if equidistant { None } else { self.opposing_handle_lengths.take() };
		let opposite = if lock_angle { None } else { self.opposite_handle_position };
		let unsnapped_delta = current_mouse - previous_mouse;
		let mut was_alt_dragging = false;

		if self.snapping_axis.is_none() {
			if self.alt_clicked_on_anchor && !self.alt_dragging_from_anchor && self.drag_start_pos.distance(input.mouse.position) > DRAG_THRESHOLD {
				// Checking which direction the dragging begins
				self.alt_dragging_from_anchor = true;
				let Some(layer) = document.network_interface.selected_nodes().selected_layers(document.metadata()).next() else {
					return;
				};
				let Some(vector_data) = document.network_interface.compute_modified_vector(layer) else { return };
				let Some(point_id) = shape_editor.selected_points().next().unwrap().get_anchor(&vector_data) else {
					return;
				};

				if vector_data.connected_count(point_id) == 2 {
					let connected_segments: Vec<HandleId> = vector_data.all_connected(point_id).collect();
					let segment1 = connected_segments[0];
					let Some(tangent1) = self.get_normalized_tangent(point_id, segment1.segment, &vector_data) else {
						return;
					};
					let segment2 = connected_segments[1];
					let Some(tangent2) = self.get_normalized_tangent(point_id, segment2.segment, &vector_data) else {
						return;
					};

					let delta = input.mouse.position - self.drag_start_pos;
					let handle = if delta.dot(tangent1) >= delta.dot(tangent2) {
						segment1.to_manipulator_point()
					} else {
						segment2.to_manipulator_point()
					};

					// Now change the selection to this handle
					shape_editor.deselect_all_points();
					shape_editor.select_points_by_manipulator_id(&vec![handle]);
					responses.add(PathToolMessage::SelectionChanged);
				}
			}

			if self.alt_dragging_from_anchor && !equidistant && self.alt_clicked_on_anchor {
				was_alt_dragging = true;
				self.alt_dragging_from_anchor = false;
				self.alt_clicked_on_anchor = false;
			}
			shape_editor.move_selected_points(handle_lengths, document, snapped_delta, equidistant, true, was_alt_dragging, opposite, responses);
			self.previous_mouse_position += document_to_viewport.inverse().transform_vector2(snapped_delta);
		} else {
			let Some(axis) = self.snapping_axis else { return };
			let projected_delta = match axis {
				Axis::X => DVec2::new(unsnapped_delta.x, 0.),
				Axis::Y => DVec2::new(0., unsnapped_delta.y),
				_ => DVec2::new(unsnapped_delta.x, 0.),
			};
			shape_editor.move_selected_points(handle_lengths, document, projected_delta, equidistant, true, false, opposite, responses);
			self.previous_mouse_position += document_to_viewport.inverse().transform_vector2(unsnapped_delta);
		}

		if snap_angle && self.snapping_axis.is_some() {
			let Some(current_axis) = self.snapping_axis else { return };
			let total_delta = self.drag_start_pos - input.mouse.position;

			if (total_delta.x.abs() > total_delta.y.abs() && current_axis == Axis::Y) || (total_delta.y.abs() > total_delta.x.abs() && current_axis == Axis::X) {
				self.stop_snap_along_axis(shape_editor, document, input, responses);
				self.start_snap_along_axis(shape_editor, document, input, responses);
			}
		}
	}
}

impl Fsm for PathToolFsmState {
	type ToolData = PathToolData;
	type ToolOptions = PathToolOptions;

	fn transition(self, event: ToolMessage, tool_data: &mut Self::ToolData, tool_action_data: &mut ToolActionHandlerData, tool_options: &Self::ToolOptions, responses: &mut VecDeque<Message>) -> Self {
		let ToolActionHandlerData { document, input, shape_editor, .. } = tool_action_data;
		let ToolMessage::Path(event) = event else { return self };
		match (self, event) {
			(_, PathToolMessage::SelectionChanged) => {
				// Set the newly targeted layers to visible
				let target_layers = document.network_interface.selected_nodes().selected_layers(document.metadata()).collect();
				shape_editor.set_selected_layers(target_layers);

				responses.add(OverlaysMessage::Draw);

				responses.add(PathToolMessage::SelectedPointUpdated);
				self
			}
			(_, PathToolMessage::Overlays(mut overlay_context)) => {
				// TODO: find the segment ids of which the selected points are a part of

				match tool_options.path_overlay_mode {
					PathOverlayMode::AllHandles => {
						path_overlays(document, DrawHandles::All, shape_editor, &mut overlay_context);
					}
					PathOverlayMode::SelectedPointHandles => {
						let selected_segments = selected_segments(document, shape_editor);

						path_overlays(document, DrawHandles::SelectedAnchors(selected_segments), shape_editor, &mut overlay_context);
					}
					PathOverlayMode::FrontierHandles => {
						let selected_segments = selected_segments(document, shape_editor);
						let selected_points = shape_editor.selected_points();
						let selected_anchors = selected_points
							.filter_map(|point_id| if let ManipulatorPointId::Anchor(p) = point_id { Some(*p) } else { None })
							.collect::<Vec<_>>();

						// Match the behavior of `PathOverlayMode::SelectedPointHandles` when only one point is selected
						if shape_editor.selected_points().count() == 1 {
							path_overlays(document, DrawHandles::SelectedAnchors(selected_segments), shape_editor, &mut overlay_context);
						} else {
							let mut segment_endpoints: HashMap<SegmentId, Vec<PointId>> = HashMap::new();

							for layer in document.network_interface.selected_nodes().selected_layers(document.metadata()) {
								let Some(vector_data) = document.network_interface.compute_modified_vector(layer) else { continue };

								// The points which are part of only one segment will be rendered
								let mut selected_segments_by_point: HashMap<PointId, Vec<SegmentId>> = HashMap::new();

								for (segment_id, _bezier, start, end) in vector_data.segment_bezier_iter() {
									if selected_segments.contains(&segment_id) {
										selected_segments_by_point.entry(start).or_default().push(segment_id);
										selected_segments_by_point.entry(end).or_default().push(segment_id);
									}
								}

								for (point, attached_segments) in selected_segments_by_point {
									if attached_segments.len() == 1 {
										segment_endpoints.entry(attached_segments[0]).or_default().push(point);
									} else if !selected_anchors.contains(&point) {
										segment_endpoints.entry(attached_segments[0]).or_default().push(point);
										segment_endpoints.entry(attached_segments[1]).or_default().push(point);
									}
								}
							}

							// Now frontier anchors can be sent for rendering overlays
							path_overlays(document, DrawHandles::FrontierHandles(segment_endpoints), shape_editor, &mut overlay_context);
						}
					}
				}

				match self {
					Self::Drawing { selection_shape } => {
						let mut fill_color = graphene_std::Color::from_rgb_str(crate::consts::COLOR_OVERLAY_BLUE.strip_prefix('#').unwrap())
							.unwrap()
							.with_alpha(0.05)
							.to_rgba_hex_srgb();
						fill_color.insert(0, '#');
						let fill_color = Some(fill_color.as_str());

						let selection_mode = match tool_action_data.preferences.get_selection_mode() {
							SelectionMode::Directional => tool_data.calculate_selection_mode_from_direction(),
							selection_mode => selection_mode,
						};

						let quad = tool_data.selection_quad();
						let polygon = &tool_data.lasso_polygon;

						match (selection_shape, selection_mode) {
							(SelectionShapeType::Box, SelectionMode::Enclosed) => overlay_context.dashed_quad(quad, fill_color, Some(4.), Some(4.), Some(0.5)),
							(SelectionShapeType::Lasso, SelectionMode::Enclosed) => overlay_context.dashed_polygon(polygon, fill_color, Some(4.), Some(4.), Some(0.5)),
							(SelectionShapeType::Box, _) => overlay_context.quad(quad, fill_color),
							(SelectionShapeType::Lasso, _) => overlay_context.polygon(polygon, fill_color),
						}
					}
					Self::Dragging(_) => {
						tool_data.snap_manager.draw_overlays(SnapData::new(document, input), &mut overlay_context);

						// Draw the snapping axis lines
						if tool_data.snapping_axis.is_some() {
							let Some(axis) = tool_data.snapping_axis else { return self };
							let origin = tool_data.drag_start_pos;
							let viewport_diagonal = input.viewport_bounds.size().length();

							let mut faded_blue = graphene_std::Color::from_rgb_str(COLOR_OVERLAY_BLUE.strip_prefix('#').unwrap())
								.unwrap()
								.with_alpha(0.25)
								.to_rgba_hex_srgb();
							faded_blue.insert(0, '#');
							let other = faded_blue.as_str();

							match axis {
								Axis::Y => {
									overlay_context.line(origin - DVec2::Y * viewport_diagonal, origin + DVec2::Y * viewport_diagonal, Some(COLOR_OVERLAY_BLUE), None);
									overlay_context.line(origin - DVec2::X * viewport_diagonal, origin + DVec2::X * viewport_diagonal, Some(other), None);
								}
								Axis::X | Axis::Both => {
									overlay_context.line(origin - DVec2::X * viewport_diagonal, origin + DVec2::X * viewport_diagonal, Some(COLOR_OVERLAY_BLUE), None);
									overlay_context.line(origin - DVec2::Y * viewport_diagonal, origin + DVec2::Y * viewport_diagonal, Some(other), None);
								}
							}
						}
					}
					Self::InsertPoint => {
						let state = tool_data.update_insertion(shape_editor, document, responses, input);

						if let Some(closest_segment) = &tool_data.segment {
							// Perpendicular line when inserting a point, and a cross when deleting a segment
							let tangent = if let (Some(handle1), Some(handle2)) = closest_segment.handle_positions(document.metadata()) {
								(handle1 - handle2).try_normalize()
							} else {
								let layer = closest_segment.layer();
								let points = closest_segment.points();
								if let Some(vector_data) = document.network_interface.compute_modified_vector(layer) {
									if let (Some(pos1), Some(pos2)) = (
										ManipulatorPointId::Anchor(points[0]).get_position(&vector_data),
										ManipulatorPointId::Anchor(points[1]).get_position(&vector_data),
									) {
										(pos1 - pos2).try_normalize()
									} else {
										None
									}
								} else {
									None
								}
							}
							.unwrap_or(DVec2::ZERO);
							let perp = tangent.perp();
							let point = closest_segment.closest_point_to_viewport();
							if tool_data.delete_segment_pressed {
								let degrees: f64 = 45.0;
								let tilted_line = DVec2::from_angle(degrees.to_radians()).rotate(tangent);
								let tilted_perp = tilted_line.perp();
								overlay_context.line(point - tilted_line * SEGMENT_OVERLAY_SIZE, point + tilted_line * SEGMENT_OVERLAY_SIZE, Some(COLOR_OVERLAY_BLUE), None);
								overlay_context.line(point - tilted_perp * SEGMENT_OVERLAY_SIZE, point + tilted_perp * SEGMENT_OVERLAY_SIZE, Some(COLOR_OVERLAY_BLUE), None);
							} else {
								overlay_context.line(point - perp * SEGMENT_OVERLAY_SIZE, point + perp * SEGMENT_OVERLAY_SIZE, Some(COLOR_OVERLAY_BLUE), None);
							}
						}

						responses.add(PathToolMessage::SelectedPointUpdated);
						return state;
					}
					_ => {}
				}

				responses.add(PathToolMessage::SelectedPointUpdated);
				self
			}

			// `Self::InsertPoint` case:
			(Self::InsertPoint, PathToolMessage::MouseDown { extend_selection, delete_segment, .. }) => {
				//| PathToolMessage::Enter { extend_selection, .. } consider adding support for ctrl key in here
				tool_data.double_click_handled = true;
				let extend_selection = input.keyboard.get(extend_selection as usize);
				let delete_segment = input.keyboard.get(delete_segment as usize);

				if delete_segment {
					if let Some(closest_segment) = &tool_data.segment {
						let segment = closest_segment.segment();
						let layer = closest_segment.layer();
						let points = closest_segment.points();
						if let Some(vector_data) = document.network_interface.compute_modified_vector(layer) {
							shape_editor.dissolve_segment(responses, layer, &vector_data, segment, points);
							responses.add(DocumentMessage::EndTransaction);
						}
					}
					return PathToolFsmState::Ready;
				} else {
					tool_data.end_insertion(shape_editor, responses, InsertEndKind::Add { extend_selection })
				}
			}
			(Self::InsertPoint, PathToolMessage::PointerMove { lock_angle, .. }) => {
				let lock_angle_state = input.keyboard.get(lock_angle as usize);
				if lock_angle_state {
					tool_data.delete_segment_pressed = true;
				} else {
					tool_data.delete_segment_pressed = false;
				}

				responses.add(OverlaysMessage::Draw);
				// `tool_data.update_insertion` would be called on `OverlaysMessage::Draw`
				// we anyway should to call it on `::Draw` because we can change scale by ctrl+scroll without `::PointerMove`

				// If there is an anchor point very close to the current point then get out the InsertPoint mode
				if shape_editor
					.find_nearest_point_indices(&document.network_interface, input.mouse.position, SELECTION_THRESHOLD)
					.is_some()
				{
					return PathToolFsmState::Ready;
				}
				self
			}
			(Self::InsertPoint, PathToolMessage::Escape | PathToolMessage::Delete | PathToolMessage::RightClick) => tool_data.end_insertion(shape_editor, responses, InsertEndKind::Abort),
			(Self::InsertPoint, PathToolMessage::GRS { key: _ }) => PathToolFsmState::InsertPoint,
			// Mouse down
			(
				_,
				PathToolMessage::MouseDown {
					direct_insert_without_sliding,
					extend_selection,
					lasso_select,
					handle_drag_from_anchor,
				},
			) => {
				let extend_selection = input.keyboard.get(extend_selection as usize);
				let lasso_select = input.keyboard.get(lasso_select as usize);
				let handle_drag_from_anchor = input.keyboard.get(handle_drag_from_anchor as usize);

				tool_data.selection_mode = None;
				tool_data.lasso_polygon.clear();

				tool_data.mouse_down(shape_editor, document, input, responses, extend_selection, lasso_select, handle_drag_from_anchor)
			}
			(
				PathToolFsmState::Drawing { selection_shape },
				PathToolMessage::PointerMove {
					equidistant,
					toggle_colinear,
					move_anchor_with_handles,
					snap_angle,
					lock_angle,
				},
			) => {
				tool_data.previous_mouse_position = input.mouse.position;

				if selection_shape == SelectionShapeType::Lasso {
					extend_lasso(&mut tool_data.lasso_polygon, input.mouse.position);
				}

				responses.add(OverlaysMessage::Draw);

				// Auto-panning
				let messages = [
					PathToolMessage::PointerOutsideViewport {
						equidistant,
						toggle_colinear,
						move_anchor_with_handles,
						snap_angle,
						lock_angle,
					}
					.into(),
					PathToolMessage::PointerMove {
						equidistant,
						toggle_colinear,
						move_anchor_with_handles,
						snap_angle,
						lock_angle,
					}
					.into(),
				];
				tool_data.auto_panning.setup_by_mouse_position(input, &messages, responses);

				PathToolFsmState::Drawing { selection_shape }
			}
			(
				PathToolFsmState::Dragging(_),
				PathToolMessage::PointerMove {
					equidistant,
					toggle_colinear,
					move_anchor_with_handles,
					snap_angle,
					lock_angle,
				},
			) => {
				let mut selected_only_handles = true;

				let selected_points = shape_editor.selected_points();

				for point in selected_points {
					if matches!(point, ManipulatorPointId::Anchor(_)) {
						selected_only_handles = false;
						break;
					}
				}

				if !tool_data.saved_points_before_handle_drag.is_empty() && (tool_data.drag_start_pos.distance(input.mouse.position) > DRAG_THRESHOLD) && (selected_only_handles) {
					tool_data.handle_drag_toggle = true;
				}

				if tool_data.selection_status.is_none() {
					if let Some(layer) = document.click(input) {
						shape_editor.select_all_anchors_in_layer(document, layer);
					}
				}

				let anchor_and_handle_toggled = input.keyboard.get(move_anchor_with_handles as usize);
				let initial_press = anchor_and_handle_toggled && !tool_data.select_anchor_toggled;
				let released_from_toggle = tool_data.select_anchor_toggled && !anchor_and_handle_toggled;

				if initial_press {
					responses.add(PathToolMessage::SelectedPointUpdated);
					tool_data.select_anchor_toggled = true;
					tool_data.save_points_before_anchor_toggle(shape_editor.selected_points().cloned().collect());
					shape_editor.select_handles_and_anchor_connected_to_current_handle(&document.network_interface);
				} else if released_from_toggle {
					responses.add(PathToolMessage::SelectedPointUpdated);
					tool_data.select_anchor_toggled = false;
					shape_editor.deselect_all_points();
					shape_editor.select_points_by_manipulator_id(&tool_data.saved_points_before_anchor_select_toggle);
					tool_data.remove_saved_points();
				}

				let toggle_colinear_state = input.keyboard.get(toggle_colinear as usize);
				let equidistant_state = input.keyboard.get(equidistant as usize);
				let lock_angle_state = input.keyboard.get(lock_angle as usize);
				let snap_angle_state = input.keyboard.get(snap_angle as usize);

				if !tool_data.update_colinear(equidistant_state, toggle_colinear_state, tool_action_data.shape_editor, tool_action_data.document, responses) {
					tool_data.drag(
						equidistant_state,
						lock_angle_state,
						snap_angle_state,
						tool_action_data.shape_editor,
						tool_action_data.document,
						input,
						responses,
					);
				}

				// Auto-panning
				let messages = [
					PathToolMessage::PointerOutsideViewport {
						toggle_colinear,
						equidistant,
						move_anchor_with_handles,
						snap_angle,
						lock_angle,
					}
					.into(),
					PathToolMessage::PointerMove {
						toggle_colinear,
						equidistant,
						move_anchor_with_handles,
						snap_angle,
						lock_angle,
					}
					.into(),
				];
				tool_data.auto_panning.setup_by_mouse_position(input, &messages, responses);

				PathToolFsmState::Dragging(tool_data.dragging_state)
			}
			(PathToolFsmState::Ready, PathToolMessage::PointerMove { lock_angle, .. }) => {
				// Check for a point in Selection threshold if it is there then don't change mode
				if shape_editor
					.find_nearest_point_indices(&document.network_interface, input.mouse.position, SELECTION_THRESHOLD)
					.is_some()
				{
					self
				}
				// Check for a segment nearby, if present then enter into insert point mode else go for ready
				else if let Some(closed_segment) = shape_editor.upper_closest_segment(&document.network_interface, input.mouse.position, SEGMENT_INSERTION_TOLERANCE) {
					let lock_angle_state = input.keyboard.get(lock_angle as usize);
					if lock_angle_state {
						tool_data.delete_segment_pressed = true;
					}
					responses.add(DocumentMessage::StartTransaction);
					tool_data.start_insertion(responses, closed_segment)
				} else {
					self
				}
			}
			(PathToolFsmState::Drawing { selection_shape: selection_type }, PathToolMessage::PointerOutsideViewport { .. }) => {
				// Auto-panning
				if let Some(offset) = tool_data.auto_panning.shift_viewport(input, responses) {
					tool_data.drag_start_pos += offset;
				}

				PathToolFsmState::Drawing { selection_shape: selection_type }
			}
			(PathToolFsmState::Dragging(dragging_state), PathToolMessage::PointerOutsideViewport { .. }) => {
				// Auto-panning
				if let Some(offset) = tool_data.auto_panning.shift_viewport(input, responses) {
					tool_data.drag_start_pos += offset;
				}

				PathToolFsmState::Dragging(dragging_state)
			}
			(
				state,
				PathToolMessage::PointerOutsideViewport {
					equidistant,
					toggle_colinear,
					move_anchor_with_handles,
					snap_angle,
					lock_angle,
				},
			) => {
				// Auto-panning
				let messages = [
					PathToolMessage::PointerOutsideViewport {
						equidistant,
						toggle_colinear,
						move_anchor_with_handles,
						snap_angle,
						lock_angle,
					}
					.into(),
					PathToolMessage::PointerMove {
						equidistant,
						toggle_colinear,
						move_anchor_with_handles,
						snap_angle,
						lock_angle,
					}
					.into(),
				];
				tool_data.auto_panning.stop(&messages, responses);

				state
			}
			(PathToolFsmState::Drawing { selection_shape }, PathToolMessage::Enter { extend_selection, shrink_selection }) => {
				let extend_selection = input.keyboard.get(extend_selection as usize);
				let shrink_selection = input.keyboard.get(shrink_selection as usize);

				let selection_change = if shrink_selection {
					SelectionChange::Shrink
				} else if extend_selection {
					SelectionChange::Extend
				} else {
					SelectionChange::Clear
				};

				if tool_data.drag_start_pos == tool_data.previous_mouse_position {
					responses.add(NodeGraphMessage::SelectedNodesSet { nodes: vec![] });
				} else {
					match selection_shape {
						SelectionShapeType::Box => {
							let bbox = [tool_data.drag_start_pos, tool_data.previous_mouse_position];
							shape_editor.select_all_in_shape(&document.network_interface, SelectionShape::Box(bbox), selection_change);
						}
						SelectionShapeType::Lasso => shape_editor.select_all_in_shape(&document.network_interface, SelectionShape::Lasso(&tool_data.lasso_polygon), selection_change),
					}
				}

				responses.add(OverlaysMessage::Draw);

				PathToolFsmState::Ready
			}
			(PathToolFsmState::Dragging { .. }, PathToolMessage::Escape | PathToolMessage::RightClick) => {
				if tool_data.handle_drag_toggle && tool_data.drag_start_pos.distance(input.mouse.position) > DRAG_THRESHOLD {
					shape_editor.deselect_all_points();
					shape_editor.select_points_by_manipulator_id(&tool_data.saved_points_before_handle_drag);

					tool_data.saved_points_before_handle_drag.clear();
					tool_data.handle_drag_toggle = false;
				}
				responses.add(DocumentMessage::AbortTransaction);
				tool_data.snap_manager.cleanup(responses);
				PathToolFsmState::Ready
			}
			(PathToolFsmState::Drawing { .. }, PathToolMessage::Escape | PathToolMessage::RightClick) => {
				tool_data.snap_manager.cleanup(responses);
				PathToolFsmState::Ready
			}
			// Mouse up
			(PathToolFsmState::Drawing { selection_shape }, PathToolMessage::DragStop { extend_selection, shrink_selection }) => {
				let extend_selection = input.keyboard.get(extend_selection as usize);
				let shrink_selection = input.keyboard.get(shrink_selection as usize);

				let select_kind = if shrink_selection {
					SelectionChange::Shrink
				} else if extend_selection {
					SelectionChange::Extend
				} else {
					SelectionChange::Clear
				};

				if tool_data.drag_start_pos == tool_data.previous_mouse_position {
					responses.add(NodeGraphMessage::SelectedNodesSet { nodes: vec![] });
				} else {
					match selection_shape {
						SelectionShapeType::Box => {
							let bbox = [tool_data.drag_start_pos, tool_data.previous_mouse_position];
							shape_editor.select_all_in_shape(&document.network_interface, SelectionShape::Box(bbox), select_kind);
						}
						SelectionShapeType::Lasso => shape_editor.select_all_in_shape(&document.network_interface, SelectionShape::Lasso(&tool_data.lasso_polygon), select_kind),
					}
				}
				responses.add(OverlaysMessage::Draw);
				responses.add(PathToolMessage::SelectedPointUpdated);

				PathToolFsmState::Ready
			}
			(_, PathToolMessage::DragStop { extend_selection, .. }) => {
				let extend_selection = input.keyboard.get(extend_selection as usize);
				let drag_occurred = tool_data.drag_start_pos.distance(input.mouse.position) > DRAG_THRESHOLD;
				let nearest_point = shape_editor.find_nearest_point_indices(&document.network_interface, input.mouse.position, SELECTION_THRESHOLD);

				if let Some((layer, nearest_point)) = nearest_point {
					if !drag_occurred && extend_selection {
						let clicked_selected = shape_editor.selected_points().any(|&point| nearest_point == point);
						if clicked_selected && tool_data.last_clicked_point_was_selected {
							shape_editor.selected_shape_state.entry(layer).or_default().deselect_point(nearest_point);
						} else {
							shape_editor.selected_shape_state.entry(layer).or_default().select_point(nearest_point);
						}
						responses.add(OverlaysMessage::Draw);
					}
				}

				if tool_data.handle_drag_toggle && drag_occurred {
					shape_editor.deselect_all_points();
					shape_editor.select_points_by_manipulator_id(&tool_data.saved_points_before_handle_drag);

					tool_data.saved_points_before_handle_drag.clear();
					tool_data.handle_drag_toggle = false;
				}

				tool_data.alt_dragging_from_anchor = false;
				tool_data.alt_clicked_on_anchor = false;

				if tool_data.select_anchor_toggled {
					shape_editor.deselect_all_points();
					shape_editor.select_points_by_manipulator_id(&tool_data.saved_points_before_anchor_select_toggle);
					tool_data.remove_saved_points();
					tool_data.select_anchor_toggled = false;
				}

				if let Some((layer, nearest_point)) = nearest_point {
					if !drag_occurred && !extend_selection {
						let clicked_selected = shape_editor.selected_points().any(|&point| nearest_point == point);
						if clicked_selected {
							shape_editor.deselect_all_points();
							shape_editor.selected_shape_state.entry(layer).or_default().select_point(nearest_point);
							responses.add(OverlaysMessage::Draw);
						}
					}
				}
				// Deselect all points if the user clicks the filled region of the shape
				else if tool_data.drag_start_pos.distance(input.mouse.position) <= DRAG_THRESHOLD {
					shape_editor.deselect_all_points();
				}

				if tool_data.snapping_axis.is_some() {
					tool_data.snapping_axis = None;
				}

				responses.add(DocumentMessage::EndTransaction);
				responses.add(PathToolMessage::SelectedPointUpdated);
				tool_data.snap_manager.cleanup(responses);
				tool_data.opposite_handle_position = None;

				PathToolFsmState::Ready
			}

			// Delete key
			(_, PathToolMessage::Delete) => {
				// Delete the selected points and clean up overlays
				responses.add(DocumentMessage::AddTransaction);
				shape_editor.delete_selected_points(document, responses);
				responses.add(PathToolMessage::SelectionChanged);

				PathToolFsmState::Ready
			}
			(_, PathToolMessage::BreakPath) => {
				shape_editor.break_path_at_selected_point(document, responses);
				PathToolFsmState::Ready
			}
			(_, PathToolMessage::DeleteAndBreakPath) => {
				shape_editor.delete_point_and_break_path(document, responses);
				PathToolFsmState::Ready
			}
			(_, PathToolMessage::FlipSmoothSharp) => {
				// Double-clicked on a point
				let nearest_point = shape_editor.find_nearest_point_indices(&document.network_interface, input.mouse.position, SELECTION_THRESHOLD);
				if nearest_point.is_some() {
					// Flip the selected point between smooth and sharp
					if !tool_data.double_click_handled && tool_data.drag_start_pos.distance(input.mouse.position) <= DRAG_THRESHOLD {
						responses.add(DocumentMessage::StartTransaction);
						shape_editor.flip_smooth_sharp(&document.network_interface, input.mouse.position, SELECTION_TOLERANCE, responses);
						responses.add(DocumentMessage::EndTransaction);
						responses.add(PathToolMessage::SelectedPointUpdated);
					}

					return PathToolFsmState::Ready;
				}

				// Double-clicked on a filled region
				if let Some(layer) = document.click(input) {
					// Select all points in the layer
					shape_editor.select_connected_anchors(document, layer, input.mouse.position);
				}

				PathToolFsmState::Ready
			}
			(_, PathToolMessage::Abort) => {
				responses.add(OverlaysMessage::Draw);
				PathToolFsmState::Ready
			}
			(_, PathToolMessage::NudgeSelectedPoints { delta_x, delta_y }) => {
				shape_editor.move_selected_points(
					tool_data.opposing_handle_lengths.take(),
					document,
					(delta_x, delta_y).into(),
					true,
					false,
					false,
					tool_data.opposite_handle_position,
					responses,
				);

				PathToolFsmState::Ready
			}
			(_, PathToolMessage::SelectAllAnchors) => {
				shape_editor.select_all_anchors_in_selected_layers(document);
				responses.add(OverlaysMessage::Draw);
				PathToolFsmState::Ready
			}
			(_, PathToolMessage::DeselectAllPoints) => {
				shape_editor.deselect_all_points();
				responses.add(OverlaysMessage::Draw);
				PathToolFsmState::Ready
			}
			(_, PathToolMessage::SelectedPointXChanged { new_x }) => {
				if let Some(&SingleSelectedPoint { coordinates, id, layer, .. }) = tool_data.selection_status.as_one() {
					shape_editor.reposition_control_point(&id, &document.network_interface, DVec2::new(new_x, coordinates.y), layer, responses);
				}
				PathToolFsmState::Ready
			}
			(_, PathToolMessage::SelectedPointYChanged { new_y }) => {
				if let Some(&SingleSelectedPoint { coordinates, id, layer, .. }) = tool_data.selection_status.as_one() {
					shape_editor.reposition_control_point(&id, &document.network_interface, DVec2::new(coordinates.x, new_y), layer, responses);
				}
				PathToolFsmState::Ready
			}
			(_, PathToolMessage::SelectedPointUpdated) => {
				let colinear = shape_editor.selected_manipulator_angles(&document.network_interface);
				tool_data.dragging_state = DraggingState {
					point_select_state: shape_editor.get_dragging_state(&document.network_interface),
					colinear,
				};
				tool_data.update_selection_status(shape_editor, document);
				self
			}
			(_, PathToolMessage::ManipulatorMakeHandlesColinear) => {
				responses.add(DocumentMessage::StartTransaction);
				shape_editor.convert_selected_manipulators_to_colinear_handles(responses, document);
				responses.add(DocumentMessage::EndTransaction);
				responses.add(PathToolMessage::SelectionChanged);
				PathToolFsmState::Ready
			}
			(_, PathToolMessage::ManipulatorMakeHandlesFree) => {
				responses.add(DocumentMessage::StartTransaction);
				shape_editor.disable_colinear_handles_state_on_selected(&document.network_interface, responses);
				responses.add(DocumentMessage::EndTransaction);
				PathToolFsmState::Ready
			}
			(_, _) => PathToolFsmState::Ready,
		}
	}

	fn update_hints(&self, responses: &mut VecDeque<Message>) {
		let hint_data = match self {
			PathToolFsmState::Ready => HintData(vec![
				HintGroup(vec![HintInfo::mouse(MouseMotion::Lmb, "Select Point"), HintInfo::keys([Key::Shift], "Extend").prepend_plus()]),
				HintGroup(vec![HintInfo::mouse(MouseMotion::LmbDrag, "Select Area"), HintInfo::keys([Key::Control], "Lasso").prepend_plus()]),
				HintGroup(vec![HintInfo::mouse(MouseMotion::Lmb, "Insert Point on Segment")]),
				// TODO: Only show if at least one anchor is selected, and dynamically show either "Smooth" or "Sharp" based on the current state
				HintGroup(vec![
					HintInfo::mouse(MouseMotion::LmbDouble, "Convert Anchor Point"),
					HintInfo::keys_and_mouse([Key::Alt], MouseMotion::Lmb, "To Sharp"),
					HintInfo::keys_and_mouse([Key::Alt], MouseMotion::LmbDrag, "To Smooth"),
				]),
				// TODO: Only show the following hints if at least one point is selected
				HintGroup(vec![HintInfo::mouse(MouseMotion::LmbDrag, "Drag Selected")]),
				HintGroup(vec![HintInfo::multi_keys([[Key::KeyG], [Key::KeyR], [Key::KeyS]], "Grab/Rotate/Scale Selected")]),
				HintGroup(vec![HintInfo::arrow_keys("Nudge Selected"), HintInfo::keys([Key::Shift], "10x").prepend_plus()]),
				HintGroup(vec![
					HintInfo::keys([Key::Delete], "Delete Selected"),
					// TODO: Only show the following hints if at least one anchor is selected
					HintInfo::keys([Key::Accel], "No Dissolve").prepend_plus(),
					HintInfo::keys([Key::Shift], "Cut Anchor").prepend_plus(),
				]),
			]),
			PathToolFsmState::Dragging(dragging_state) => {
				let colinear = dragging_state.colinear;
				let mut dragging_hint_data = HintData(Vec::new());
				dragging_hint_data
					.0
					.push(HintGroup(vec![HintInfo::mouse(MouseMotion::Rmb, ""), HintInfo::keys([Key::Escape], "Cancel").prepend_slash()]));

				let drag_anchor = HintInfo::keys([Key::Space], "Drag Anchor");
				let toggle_group = match dragging_state.point_select_state {
					PointSelectState::HandleNoPair | PointSelectState::HandleWithPair => {
						let mut hints = vec![HintInfo::keys([Key::Tab], "Swap Dragged Handle")];
						hints.push(HintInfo::keys(
							[Key::KeyC],
							if colinear == ManipulatorAngle::Colinear {
								"Break Colinear Handles"
							} else {
								"Make Handles Colinear"
							},
						));
						hints
					}
					PointSelectState::Anchor => Vec::new(),
				};
				let hold_group = match dragging_state.point_select_state {
					PointSelectState::HandleNoPair => {
						let mut hints = vec![];
						if colinear != ManipulatorAngle::Free {
							hints.push(HintInfo::keys([Key::Alt], "Equidistant Handles"));
						}
						hints.push(HintInfo::keys([Key::Shift], "15° Increments"));
						hints.push(HintInfo::keys([Key::Control], "Lock Angle"));
						hints.push(drag_anchor);
						hints
					}
					PointSelectState::HandleWithPair => {
						let mut hints = vec![];
						if colinear != ManipulatorAngle::Free {
							hints.push(HintInfo::keys([Key::Alt], "Equidistant Handles"));
						}
						hints.push(HintInfo::keys([Key::Shift], "15° Increments"));
						hints.push(HintInfo::keys([Key::Control], "Lock Angle"));
						hints.push(drag_anchor);
						hints
					}
					PointSelectState::Anchor => Vec::new(),
				};

				if !toggle_group.is_empty() {
					dragging_hint_data.0.push(HintGroup(toggle_group));
				}

				if !hold_group.is_empty() {
					dragging_hint_data.0.push(HintGroup(hold_group));
				}

				dragging_hint_data
			}
			PathToolFsmState::Drawing { .. } => HintData(vec![
				HintGroup(vec![HintInfo::mouse(MouseMotion::Rmb, ""), HintInfo::keys([Key::Escape], "Cancel").prepend_slash()]),
				HintGroup(vec![
					HintInfo::mouse(MouseMotion::LmbDrag, "Select Area"),
					HintInfo::keys([Key::Shift], "Extend").prepend_plus(),
					HintInfo::keys([Key::Alt], "Subtract").prepend_plus(),
				]),
			]),
			PathToolFsmState::InsertPoint => HintData(vec![
				HintGroup(vec![HintInfo::mouse(MouseMotion::Rmb, ""), HintInfo::keys([Key::Escape], "Cancel").prepend_slash()]),
				HintGroup(vec![HintInfo::mouse(MouseMotion::Lmb, "Insert Point")]),
			]),
		};

		responses.add(FrontendMessage::UpdateInputHints { hint_data });
	}

	fn update_cursor(&self, responses: &mut VecDeque<Message>) {
		responses.add(FrontendMessage::UpdateMouseCursor { cursor: MouseCursorIcon::Default });
	}
}

#[derive(Debug, PartialEq, Default)]
enum SelectionStatus {
	#[default]
	None,
	One(SingleSelectedPoint),
	Multiple(MultipleSelectedPoints),
}

impl SelectionStatus {
	fn is_none(&self) -> bool {
		self == &SelectionStatus::None
	}

	fn as_one(&self) -> Option<&SingleSelectedPoint> {
		match self {
			SelectionStatus::One(one) => Some(one),
			_ => None,
		}
	}

	fn angle(&self) -> Option<ManipulatorAngle> {
		match self {
			Self::None => None,
			Self::One(one) => Some(one.manipulator_angle),
			Self::Multiple(one) => Some(one.manipulator_angle),
		}
	}
}

#[derive(Debug, PartialEq)]
struct MultipleSelectedPoints {
	manipulator_angle: ManipulatorAngle,
}

#[derive(Debug, PartialEq)]
struct SingleSelectedPoint {
	coordinates: DVec2,
	id: ManipulatorPointId,
	layer: LayerNodeIdentifier,
	manipulator_angle: ManipulatorAngle,
}

/// Sets the cumulative description of the selected points: if `None` are selected, if `One` is selected, or if `Multiple` are selected.
/// Applies to any selected points, whether they are anchors or handles; and whether they are from a single shape or across multiple shapes.
fn get_selection_status(network_interface: &NodeNetworkInterface, shape_state: &mut ShapeState) -> SelectionStatus {
	let mut selection_layers = shape_state.selected_shape_state.iter().map(|(k, v)| (*k, v.selected_points_count()));
	let total_selected_points = selection_layers.clone().map(|(_, v)| v).sum::<usize>();

	// Check to see if only one manipulator group in a single shape is selected
	if total_selected_points == 1 {
		let Some(layer) = selection_layers.find(|(_, v)| *v > 0).map(|(k, _)| k) else {
			return SelectionStatus::None;
		};
		let Some(vector_data) = network_interface.compute_modified_vector(layer) else {
			return SelectionStatus::None;
		};
		let Some(&point) = shape_state.selected_points().next() else {
			return SelectionStatus::None;
		};
		let Some(local_position) = point.get_position(&vector_data) else {
			return SelectionStatus::None;
		};

		let coordinates = network_interface.document_metadata().transform_to_document(layer).transform_point2(local_position);
		let manipulator_angle = if vector_data.colinear(point) { ManipulatorAngle::Colinear } else { ManipulatorAngle::Free };

		return SelectionStatus::One(SingleSelectedPoint {
			coordinates,
			layer,
			id: point,
			manipulator_angle,
		});
	};

	// Check to see if multiple manipulator groups are selected
	if total_selected_points > 1 {
		return SelectionStatus::Multiple(MultipleSelectedPoints {
			manipulator_angle: shape_state.selected_manipulator_angles(network_interface),
		});
	}

	SelectionStatus::None
}
