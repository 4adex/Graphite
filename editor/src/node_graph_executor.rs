use crate::consts::FILE_SAVE_SUFFIX;
use crate::messages::animation::TimingInformation;
use crate::messages::frontend::utility_types::{ExportBounds, FileType};
use crate::messages::portfolio::document::utility_types::document_metadata::LayerNodeIdentifier;
use crate::messages::portfolio::document::utility_types::network_interface::NodeNetworkInterface;
use crate::messages::prelude::*;
use crate::messages::tool::common_functionality::graph_modification_utils::NodeGraphLayer;
use glam::{DAffine2, DVec2, UVec2};
use graph_craft::concrete;
use graph_craft::document::value::{RenderOutput, TaggedValue};
use graph_craft::document::{DocumentNode, DocumentNodeImplementation, NodeId, NodeInput, NodeNetwork, generate_uuid};
use graph_craft::graphene_compiler::Compiler;
use graph_craft::proto::GraphErrors;
use graph_craft::wasm_application_io::EditorPreferences;
use graphene_core::Context;
use graphene_core::application_io::{NodeGraphUpdateMessage, NodeGraphUpdateSender, RenderConfig};
use graphene_core::memo::IORecord;
use graphene_core::renderer::{GraphicElementRendered, RenderParams, SvgRender};
use graphene_core::renderer::{RenderSvgSegmentList, SvgSegment};
use graphene_core::text::FontCache;
use graphene_core::transform::Footprint;
use graphene_core::vector::style::ViewMode;
use graphene_std::renderer::{RenderMetadata, format_transform_matrix};
use graphene_std::vector::{VectorData, VectorDataTable};
use graphene_std::wasm_application_io::{WasmApplicationIo, WasmEditorApi};
use interpreted_executor::dynamic_executor::{DynamicExecutor, IntrospectError, ResolvedDocumentNodeTypesDelta};
use interpreted_executor::util::wrap_network_in_scope;
use once_cell::sync::Lazy;
use spin::Mutex;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};

/// Persistent data between graph executions. It's updated via message passing from the editor thread with [`NodeRuntimeMessage`]`.
/// Some of these fields are put into a [`WasmEditorApi`] which is passed to the final compiled graph network upon each execution.
/// Once the implementation is finished, this will live in a separate thread. Right now it's part of the main JS thread, but its own separate JS stack frame independent from the editor.
pub struct NodeRuntime {
	executor: DynamicExecutor,
	receiver: Receiver<NodeRuntimeMessage>,
	sender: InternalNodeGraphUpdateSender,
	editor_preferences: EditorPreferences,
	old_graph: Option<NodeNetwork>,
	update_thumbnails: bool,

	editor_api: Arc<WasmEditorApi>,
	node_graph_errors: GraphErrors,
	monitor_nodes: Vec<Vec<NodeId>>,

	/// Which node is inspected and which monitor node is used (if any) for the current execution
	inspect_state: Option<InspectState>,

	// TODO: Remove, it doesn't need to be persisted anymore
	/// The current renders of the thumbnails for layer nodes.
	thumbnail_renders: HashMap<NodeId, Vec<SvgSegment>>,
	vector_modify: HashMap<NodeId, VectorData>,
}

/// Messages passed from the editor thread to the node runtime thread.
pub enum NodeRuntimeMessage {
	GraphUpdate(GraphUpdate),
	ExecutionRequest(ExecutionRequest),
	FontCacheUpdate(FontCache),
	EditorPreferencesUpdate(EditorPreferences),
}

#[derive(Default, Debug, Clone)]
pub struct ExportConfig {
	pub file_name: String,
	pub file_type: FileType,
	pub scale_factor: f64,
	pub bounds: ExportBounds,
	pub transparent_background: bool,
	pub size: DVec2,
}

pub struct GraphUpdate {
	network: NodeNetwork,
	/// The node that should be temporary inspected during execution
	inspect_node: Option<NodeId>,
}

pub struct ExecutionRequest {
	execution_id: u64,
	render_config: RenderConfig,
}

pub struct ExecutionResponse {
	execution_id: u64,
	result: Result<TaggedValue, String>,
	responses: VecDeque<FrontendMessage>,
	transform: DAffine2,
	vector_modify: HashMap<NodeId, VectorData>,
	/// The resulting value from the temporary inspected during execution
	inspect_result: Option<InspectResult>,
}

pub struct CompilationResponse {
	result: Result<ResolvedDocumentNodeTypesDelta, String>,
	node_graph_errors: GraphErrors,
}

pub enum NodeGraphUpdate {
	ExecutionResponse(ExecutionResponse),
	CompilationResponse(CompilationResponse),
	NodeGraphUpdateMessage(NodeGraphUpdateMessage),
}

#[derive(Clone)]
struct InternalNodeGraphUpdateSender(Sender<NodeGraphUpdate>);

impl InternalNodeGraphUpdateSender {
	fn send_generation_response(&self, response: CompilationResponse) {
		self.0.send(NodeGraphUpdate::CompilationResponse(response)).expect("Failed to send response")
	}

	fn send_execution_response(&self, response: ExecutionResponse) {
		self.0.send(NodeGraphUpdate::ExecutionResponse(response)).expect("Failed to send response")
	}
}

impl NodeGraphUpdateSender for InternalNodeGraphUpdateSender {
	fn send(&self, message: NodeGraphUpdateMessage) {
		self.0.send(NodeGraphUpdate::NodeGraphUpdateMessage(message)).expect("Failed to send response")
	}
}

pub static NODE_RUNTIME: Lazy<Mutex<Option<NodeRuntime>>> = Lazy::new(|| Mutex::new(None));

impl NodeRuntime {
	pub fn new(receiver: Receiver<NodeRuntimeMessage>, sender: Sender<NodeGraphUpdate>) -> Self {
		Self {
			executor: DynamicExecutor::default(),
			receiver,
			sender: InternalNodeGraphUpdateSender(sender.clone()),
			editor_preferences: EditorPreferences::default(),
			old_graph: None,
			update_thumbnails: true,

			editor_api: WasmEditorApi {
				font_cache: FontCache::default(),
				editor_preferences: Box::new(EditorPreferences::default()),
				node_graph_message_sender: Box::new(InternalNodeGraphUpdateSender(sender)),

				application_io: None,
			}
			.into(),

			node_graph_errors: Vec::new(),
			monitor_nodes: Vec::new(),

			inspect_state: None,

			thumbnail_renders: Default::default(),
			vector_modify: Default::default(),
		}
	}

	pub async fn run(&mut self) {
		if self.editor_api.application_io.is_none() {
			self.editor_api = WasmEditorApi {
				application_io: Some(WasmApplicationIo::new().await.into()),
				font_cache: self.editor_api.font_cache.clone(),
				node_graph_message_sender: Box::new(self.sender.clone()),
				editor_preferences: Box::new(self.editor_preferences.clone()),
			}
			.into();
		}

		let mut font = None;
		let mut preferences = None;
		let mut graph = None;
		let mut execution = None;
		for request in self.receiver.try_iter() {
			match request {
				NodeRuntimeMessage::GraphUpdate(_) => graph = Some(request),
				NodeRuntimeMessage::ExecutionRequest(_) => execution = Some(request),
				NodeRuntimeMessage::FontCacheUpdate(_) => font = Some(request),
				NodeRuntimeMessage::EditorPreferencesUpdate(_) => preferences = Some(request),
			}
		}
		let requests = [font, preferences, graph, execution].into_iter().flatten();

		for request in requests {
			match request {
				NodeRuntimeMessage::FontCacheUpdate(font_cache) => {
					self.editor_api = WasmEditorApi {
						font_cache,
						application_io: self.editor_api.application_io.clone(),
						node_graph_message_sender: Box::new(self.sender.clone()),
						editor_preferences: Box::new(self.editor_preferences.clone()),
					}
					.into();
					if let Some(graph) = self.old_graph.clone() {
						// We ignore this result as compilation errors should have been reported in an earlier iteration
						let _ = self.update_network(graph).await;
					}
				}
				NodeRuntimeMessage::EditorPreferencesUpdate(preferences) => {
					self.editor_preferences = preferences.clone();
					self.editor_api = WasmEditorApi {
						font_cache: self.editor_api.font_cache.clone(),
						application_io: self.editor_api.application_io.clone(),
						node_graph_message_sender: Box::new(self.sender.clone()),
						editor_preferences: Box::new(preferences),
					}
					.into();
					if let Some(graph) = self.old_graph.clone() {
						// We ignore this result as compilation errors should have been reported in an earlier iteration
						let _ = self.update_network(graph).await;
					}
				}
				NodeRuntimeMessage::GraphUpdate(GraphUpdate { mut network, inspect_node }) => {
					// Insert the monitor node to manage the inspection
					self.inspect_state = inspect_node.map(|inspect| InspectState::monitor_inspect_node(&mut network, inspect));

					self.old_graph = Some(network.clone());
					self.node_graph_errors.clear();
					let result = self.update_network(network).await;
					self.update_thumbnails = true;
					self.sender.send_generation_response(CompilationResponse {
						result,
						node_graph_errors: self.node_graph_errors.clone(),
					});
				}
				NodeRuntimeMessage::ExecutionRequest(ExecutionRequest { execution_id, render_config, .. }) => {
					let transform = render_config.viewport.transform;

					let result = self.execute_network(render_config).await;
					let mut responses = VecDeque::new();
					// TODO: Only process monitor nodes if the graph has changed, not when only the Footprint changes
					self.process_monitor_nodes(&mut responses, self.update_thumbnails);
					self.update_thumbnails = false;

					// Resolve the result from the inspection by accessing the monitor node
					let inspect_result = self.inspect_state.and_then(|state| state.access(&self.executor));

					self.sender.send_execution_response(ExecutionResponse {
						execution_id,
						result,
						responses,
						transform,
						vector_modify: self.vector_modify.clone(),
						inspect_result,
					});
				}
			}
		}
	}

	async fn update_network(&mut self, graph: NodeNetwork) -> Result<ResolvedDocumentNodeTypesDelta, String> {
		let scoped_network = wrap_network_in_scope(graph, self.editor_api.clone());

		// We assume only one output
		assert_eq!(scoped_network.exports.len(), 1, "Graph with multiple outputs not yet handled");
		let c = Compiler {};
		let proto_network = match c.compile_single(scoped_network) {
			Ok(network) => network,
			Err(e) => return Err(e),
		};
		self.monitor_nodes = proto_network
			.nodes
			.iter()
			.filter(|(_, node)| node.identifier == "graphene_core::memo::MonitorNode".into())
			.map(|(_, node)| node.original_location.path.clone().unwrap_or_default())
			.collect::<Vec<_>>();

		assert_ne!(proto_network.nodes.len(), 0, "No proto nodes exist?");
		self.executor.update(proto_network).await.map_err(|e| {
			self.node_graph_errors.clone_from(&e);
			format!("{e:?}")
		})
	}

	async fn execute_network(&mut self, render_config: RenderConfig) -> Result<TaggedValue, String> {
		use graph_craft::graphene_compiler::Executor;

		let result = match self.executor.input_type() {
			Some(t) if t == concrete!(RenderConfig) => (&self.executor).execute(render_config).await.map_err(|e| e.to_string()),
			Some(t) if t == concrete!(()) => (&self.executor).execute(()).await.map_err(|e| e.to_string()),
			Some(t) => Err(format!("Invalid input type {t:?}")),
			_ => Err(format!("No input type:\n{:?}", self.node_graph_errors)),
		};
		let result = match result {
			Ok(value) => value,
			Err(e) => return Err(e),
		};

		Ok(result)
	}

	/// Updates state data
	pub fn process_monitor_nodes(&mut self, responses: &mut VecDeque<FrontendMessage>, update_thumbnails: bool) {
		// TODO: Consider optimizing this since it's currently O(m*n^2), with a sort it could be made O(m * n*log(n))
		self.thumbnail_renders.retain(|id, _| self.monitor_nodes.iter().any(|monitor_node_path| monitor_node_path.contains(id)));

		for monitor_node_path in &self.monitor_nodes {
			// Skip the inspect monitor node
			if self.inspect_state.is_some_and(|inspect_state| monitor_node_path.last().copied() == Some(inspect_state.monitor_node)) {
				continue;
			}
			// The monitor nodes are located within a document node, and are thus children in that network, so this gets the parent document node's ID
			let Some(parent_network_node_id) = monitor_node_path.len().checked_sub(2).and_then(|index| monitor_node_path.get(index)).copied() else {
				warn!("Monitor node has invalid node id");

				continue;
			};

			// Extract the monitor node's stored `GraphicElement` data.
			let Ok(introspected_data) = self.executor.introspect(monitor_node_path) else {
				// TODO: Fix the root of the issue causing the spam of this warning (this at least temporarily disables it in release builds)
				#[cfg(debug_assertions)]
				warn!("Failed to introspect monitor node {}", self.executor.introspect(monitor_node_path).unwrap_err());

				continue;
			};

			if let Some(io) = introspected_data.downcast_ref::<IORecord<Context, graphene_core::GraphicElement>>() {
				Self::process_graphic_element(&mut self.thumbnail_renders, parent_network_node_id, &io.output, responses, update_thumbnails)
			} else if let Some(io) = introspected_data.downcast_ref::<IORecord<(), graphene_core::GraphicElement>>() {
				Self::process_graphic_element(&mut self.thumbnail_renders, parent_network_node_id, &io.output, responses, update_thumbnails)
			} else if let Some(io) = introspected_data.downcast_ref::<IORecord<Context, graphene_core::Artboard>>() {
				Self::process_graphic_element(&mut self.thumbnail_renders, parent_network_node_id, &io.output, responses, update_thumbnails)
			} else if let Some(io) = introspected_data.downcast_ref::<IORecord<(), graphene_core::Artboard>>() {
				Self::process_graphic_element(&mut self.thumbnail_renders, parent_network_node_id, &io.output, responses, update_thumbnails)
			}
			// Insert the vector modify if we are dealing with vector data
			else if let Some(record) = introspected_data.downcast_ref::<IORecord<Context, VectorDataTable>>() {
				self.vector_modify.insert(parent_network_node_id, record.output.one_instance().instance.clone());
			} else if let Some(record) = introspected_data.downcast_ref::<IORecord<(), VectorDataTable>>() {
				self.vector_modify.insert(parent_network_node_id, record.output.one_instance().instance.clone());
			}
		}
	}

	// If this is `GraphicElement` data:
	// Regenerate click targets and thumbnails for the layers in the graph, modifying the state and updating the UI.
	fn process_graphic_element(
		thumbnail_renders: &mut HashMap<NodeId, Vec<SvgSegment>>,
		parent_network_node_id: NodeId,
		graphic_element: &impl GraphicElementRendered,
		responses: &mut VecDeque<FrontendMessage>,
		update_thumbnails: bool,
	) {
		// RENDER THUMBNAIL

		if !update_thumbnails {
			return;
		}

		let bounds = graphic_element.bounding_box(DAffine2::IDENTITY);

		// Render the thumbnail from a `GraphicElement` into an SVG string
		let render_params = RenderParams::new(ViewMode::Normal, bounds, true, false, false);
		let mut render = SvgRender::new();
		graphic_element.render_svg(&mut render, &render_params);

		// And give the SVG a viewbox and outer <svg>...</svg> wrapper tag
		let [min, max] = bounds.unwrap_or_default();
		render.format_svg(min, max);

		// UPDATE FRONTEND THUMBNAIL

		let new_thumbnail_svg = render.svg;
		let old_thumbnail_svg = thumbnail_renders.entry(parent_network_node_id).or_default();

		if old_thumbnail_svg != &new_thumbnail_svg {
			responses.push_back(FrontendMessage::UpdateNodeThumbnail {
				id: parent_network_node_id,
				value: new_thumbnail_svg.to_svg_string(),
			});
			*old_thumbnail_svg = new_thumbnail_svg;
		}
	}
}

pub async fn introspect_node(path: &[NodeId]) -> Result<Arc<dyn std::any::Any + Send + Sync + 'static>, IntrospectError> {
	let runtime = NODE_RUNTIME.lock();
	if let Some(ref mut runtime) = runtime.as_ref() {
		return runtime.executor.introspect(path);
	}
	Err(IntrospectError::RuntimeNotReady)
}

pub async fn run_node_graph() -> bool {
	let Some(mut runtime) = NODE_RUNTIME.try_lock() else { return false };
	if let Some(ref mut runtime) = runtime.as_mut() {
		runtime.run().await;
	}
	true
}

pub async fn replace_node_runtime(runtime: NodeRuntime) -> Option<NodeRuntime> {
	let mut node_runtime = NODE_RUNTIME.lock();
	node_runtime.replace(runtime)
}

#[derive(Debug)]
pub struct NodeGraphExecutor {
	sender: Sender<NodeRuntimeMessage>,
	receiver: Receiver<NodeGraphUpdate>,
	futures: HashMap<u64, ExecutionContext>,
	node_graph_hash: u64,
	old_inspect_node: Option<NodeId>,
}

/// Which node is inspected and which monitor node is used (if any) for the current execution
#[derive(Debug, Clone, Copy)]
struct InspectState {
	inspect_node: NodeId,
	monitor_node: NodeId,
}

/// The resulting value from the temporary inspected during execution
#[derive(Clone, Debug, Default)]
pub struct InspectResult {
	pub introspected_data: Option<Arc<dyn std::any::Any + Send + Sync + 'static>>,
	pub inspect_node: NodeId,
}

// This is very ugly but is required to be inside a message
impl PartialEq for InspectResult {
	fn eq(&self, other: &Self) -> bool {
		self.inspect_node == other.inspect_node
	}
}

impl InspectState {
	/// Insert the monitor node to manage the inspection
	pub fn monitor_inspect_node(network: &mut NodeNetwork, inspect_node: NodeId) -> Self {
		let monitor_id = NodeId::new();

		// It is necessary to replace the inputs before inserting the monitor node to avoid changing the input of the new monitor node
		for input in network.nodes.values_mut().flat_map(|node| node.inputs.iter_mut()).chain(&mut network.exports) {
			let NodeInput::Node { node_id, output_index, .. } = input else { continue };
			// We only care about the primary output of our inspect node
			if *output_index != 0 || *node_id != inspect_node {
				continue;
			}

			*node_id = monitor_id;
		}

		let monitor_node = DocumentNode {
			inputs: vec![NodeInput::node(inspect_node, 0)], // Connect to the primary output of the inspect node
			implementation: DocumentNodeImplementation::proto("graphene_core::memo::MonitorNode"),
			manual_composition: Some(graph_craft::generic!(T)),
			skip_deduplication: true,
			..Default::default()
		};
		network.nodes.insert(monitor_id, monitor_node);

		Self {
			inspect_node,
			monitor_node: monitor_id,
		}
	}

	/// Resolve the result from the inspection by accessing the monitor node
	fn access(&self, executor: &DynamicExecutor) -> Option<InspectResult> {
		let introspected_data = executor.introspect(&[self.monitor_node]).inspect_err(|e| warn!("Failed to introspect monitor node {e}")).ok();

		Some(InspectResult {
			inspect_node: self.inspect_node,
			introspected_data,
		})
	}
}

#[derive(Debug, Clone)]
struct ExecutionContext {
	export_config: Option<ExportConfig>,
}

impl Default for NodeGraphExecutor {
	fn default() -> Self {
		let (request_sender, request_receiver) = std::sync::mpsc::channel();
		let (response_sender, response_receiver) = std::sync::mpsc::channel();
		futures::executor::block_on(replace_node_runtime(NodeRuntime::new(request_receiver, response_sender)));

		Self {
			futures: Default::default(),
			sender: request_sender,
			receiver: response_receiver,
			node_graph_hash: 0,
			old_inspect_node: None,
		}
	}
}

impl NodeGraphExecutor {
	/// A local runtime is useful on threads since having global state causes flakes
	#[cfg(test)]
	pub(crate) fn new_with_local_runtime() -> (NodeRuntime, Self) {
		let (request_sender, request_receiver) = std::sync::mpsc::channel();
		let (response_sender, response_receiver) = std::sync::mpsc::channel();
		let node_runtime = NodeRuntime::new(request_receiver, response_sender);

		let node_executor = Self {
			futures: Default::default(),
			sender: request_sender,
			receiver: response_receiver,
			node_graph_hash: 0,
			old_inspect_node: None,
		};
		(node_runtime, node_executor)
	}

	/// Execute the network by flattening it and creating a borrow stack.
	fn queue_execution(&self, render_config: RenderConfig) -> u64 {
		let execution_id = generate_uuid();
		let request = ExecutionRequest { execution_id, render_config };
		self.sender.send(NodeRuntimeMessage::ExecutionRequest(request)).expect("Failed to send generation request");

		execution_id
	}

	pub async fn introspect_node(&self, path: &[NodeId]) -> Result<Arc<dyn std::any::Any + Send + Sync + 'static>, IntrospectError> {
		introspect_node(path).await
	}

	pub fn update_font_cache(&self, font_cache: FontCache) {
		self.sender.send(NodeRuntimeMessage::FontCacheUpdate(font_cache)).expect("Failed to send font cache update");
	}

	pub fn update_editor_preferences(&self, editor_preferences: EditorPreferences) {
		self.sender
			.send(NodeRuntimeMessage::EditorPreferencesUpdate(editor_preferences))
			.expect("Failed to send editor preferences");
	}

	pub fn introspect_node_in_network<T: std::any::Any + core::fmt::Debug, U, F1: FnOnce(&NodeNetwork) -> Option<NodeId>, F2: FnOnce(&T) -> U>(
		&mut self,
		network: &NodeNetwork,
		node_path: &[NodeId],
		find_node: F1,
		extract_data: F2,
	) -> Option<U> {
		let wrapping_document_node = network.nodes.get(node_path.last()?)?;
		let DocumentNodeImplementation::Network(wrapped_network) = &wrapping_document_node.implementation else {
			return None;
		};
		let introspection_node = find_node(wrapped_network)?;
		let introspection = futures::executor::block_on(self.introspect_node(&[node_path, &[introspection_node]].concat())).ok()?;
		let Some(downcasted): Option<&T> = <dyn std::any::Any>::downcast_ref(introspection.as_ref()) else {
			log::warn!("Failed to downcast type for introspection");
			return None;
		};
		Some(extract_data(downcasted))
	}

	/// Updates the network to monitor all inputs. Useful for the testing.
	#[cfg(test)]
	pub(crate) fn update_node_graph_instrumented(&mut self, document: &mut DocumentMessageHandler) -> Result<Instrumented, String> {
		// We should always invalidate the cache.
		self.node_graph_hash = generate_uuid();
		let mut network = document.network_interface.document_network().clone();
		let instrumented = Instrumented::new(&mut network);

		self.sender
			.send(NodeRuntimeMessage::GraphUpdate(GraphUpdate { network, inspect_node: None }))
			.map_err(|e| e.to_string())?;
		Ok(instrumented)
	}

	/// Update the cached network if necessary.
	fn update_node_graph(&mut self, document: &mut DocumentMessageHandler, inspect_node: Option<NodeId>, ignore_hash: bool) -> Result<(), String> {
		let network_hash = document.network_interface.document_network().current_hash();
		// Refresh the graph when it changes or the inspect node changes
		if network_hash != self.node_graph_hash || self.old_inspect_node != inspect_node || ignore_hash {
			let network = document.network_interface.document_network().clone();
			self.old_inspect_node = inspect_node;
			self.node_graph_hash = network_hash;

			self.sender.send(NodeRuntimeMessage::GraphUpdate(GraphUpdate { network, inspect_node })).map_err(|e| e.to_string())?;
		}
		Ok(())
	}

	/// Adds an evaluate request for whatever current network is cached.
	pub(crate) fn submit_current_node_graph_evaluation(&mut self, document: &mut DocumentMessageHandler, viewport_resolution: UVec2, time: TimingInformation) -> Result<(), String> {
		let render_config = RenderConfig {
			viewport: Footprint {
				transform: document.metadata().document_to_viewport,
				resolution: viewport_resolution,
				..Default::default()
			},
			time,
			#[cfg(any(feature = "resvg", feature = "vello"))]
			export_format: graphene_core::application_io::ExportFormat::Canvas,
			#[cfg(not(any(feature = "resvg", feature = "vello")))]
			export_format: graphene_core::application_io::ExportFormat::Svg,
			view_mode: document.view_mode,
			hide_artboards: false,
			for_export: false,
		};

		// Execute the node graph
		let execution_id = self.queue_execution(render_config);

		self.futures.insert(execution_id, ExecutionContext { export_config: None });
		Ok(())
	}

	/// Evaluates a node graph, computing the entire graph
	pub fn submit_node_graph_evaluation(
		&mut self,
		document: &mut DocumentMessageHandler,
		viewport_resolution: UVec2,
		time: TimingInformation,
		inspect_node: Option<NodeId>,
		ignore_hash: bool,
	) -> Result<(), String> {
		self.update_node_graph(document, inspect_node, ignore_hash)?;
		self.submit_current_node_graph_evaluation(document, viewport_resolution, time)?;

		Ok(())
	}

	/// Evaluates a node graph for export
	pub fn submit_document_export(&mut self, document: &mut DocumentMessageHandler, mut export_config: ExportConfig) -> Result<(), String> {
		let network = document.network_interface.document_network().clone();

		// Calculate the bounding box of the region to be exported
		let bounds = match export_config.bounds {
			ExportBounds::AllArtwork => document.network_interface.document_bounds_document_space(!export_config.transparent_background),
			ExportBounds::Selection => document.network_interface.selected_bounds_document_space(!export_config.transparent_background, &[]),
			ExportBounds::Artboard(id) => document.metadata().bounding_box_document(id),
		}
		.ok_or_else(|| "No bounding box".to_string())?;
		let size = bounds[1] - bounds[0];
		let transform = DAffine2::from_translation(bounds[0]).inverse();

		let render_config = RenderConfig {
			viewport: Footprint {
				transform: DAffine2::from_scale(DVec2::splat(export_config.scale_factor)) * transform,
				resolution: (size * export_config.scale_factor).as_uvec2(),
				..Default::default()
			},
			time: Default::default(),
			export_format: graphene_core::application_io::ExportFormat::Svg,
			view_mode: document.view_mode,
			hide_artboards: export_config.transparent_background,
			for_export: true,
		};
		export_config.size = size;

		// Execute the node graph
		self.sender
			.send(NodeRuntimeMessage::GraphUpdate(GraphUpdate { network, inspect_node: None }))
			.map_err(|e| e.to_string())?;
		let execution_id = self.queue_execution(render_config);
		let execution_context = ExecutionContext { export_config: Some(export_config) };
		self.futures.insert(execution_id, execution_context);

		Ok(())
	}

	fn export(&self, node_graph_output: TaggedValue, export_config: ExportConfig, responses: &mut VecDeque<Message>) -> Result<(), String> {
		let TaggedValue::RenderOutput(RenderOutput {
			data: graphene_std::wasm_application_io::RenderOutputType::Svg(svg),
			..
		}) = node_graph_output
		else {
			return Err("Incorrect render type for exporting (expected RenderOutput::Svg)".to_string());
		};

		let ExportConfig {
			file_type,
			file_name,
			size,
			scale_factor,
			..
		} = export_config;

		let file_suffix = &format!(".{file_type:?}").to_lowercase();
		let name = match file_name.ends_with(FILE_SAVE_SUFFIX) {
			true => file_name.replace(FILE_SAVE_SUFFIX, file_suffix),
			false => file_name + file_suffix,
		};

		if file_type == FileType::Svg {
			responses.add(FrontendMessage::TriggerDownloadTextFile { document: svg, name });
		} else {
			let mime = file_type.to_mime().to_string();
			let size = (size * scale_factor).into();
			responses.add(FrontendMessage::TriggerDownloadImage { svg, name, mime, size });
		}
		Ok(())
	}

	pub fn poll_node_graph_evaluation(&mut self, document: &mut DocumentMessageHandler, responses: &mut VecDeque<Message>) -> Result<(), String> {
		let results = self.receiver.try_iter().collect::<Vec<_>>();
		for response in results {
			match response {
				NodeGraphUpdate::ExecutionResponse(execution_response) => {
					let ExecutionResponse {
						execution_id,
						result,
						responses: existing_responses,
						transform,
						vector_modify,
						inspect_result,
					} = execution_response;

					responses.add(OverlaysMessage::Draw);

					let node_graph_output = match result {
						Ok(output) => output,
						Err(e) => {
							// Clear the click targets while the graph is in an un-renderable state
							document.network_interface.update_click_targets(HashMap::new());
							document.network_interface.update_vector_modify(HashMap::new());
							return Err(format!("Node graph evaluation failed:\n{e}"));
						}
					};

					responses.extend(existing_responses.into_iter().map(Into::into));
					document.network_interface.update_vector_modify(vector_modify);

					let execution_context = self.futures.remove(&execution_id).ok_or_else(|| "Invalid generation ID".to_string())?;
					if let Some(export_config) = execution_context.export_config {
						// Special handling for exporting the artwork
						self.export(node_graph_output, export_config, responses)?
					} else {
						self.process_node_graph_output(node_graph_output, transform, responses)?
					}

					// Update the spreadsheet on the frontend using the value of the inspect result.
					if self.old_inspect_node.is_some() {
						if let Some(inspect_result) = inspect_result {
							responses.add(SpreadsheetMessage::UpdateLayout { inspect_result });
						}
					}
				}
				NodeGraphUpdate::CompilationResponse(execution_response) => {
					let CompilationResponse { node_graph_errors, result } = execution_response;
					let type_delta = match result {
						Err(e) => {
							// Clear the click targets while the graph is in an un-renderable state

							document.network_interface.update_click_targets(HashMap::new());
							document.network_interface.update_vector_modify(HashMap::new());

							log::trace!("{e}");

							responses.add(NodeGraphMessage::UpdateTypes {
								resolved_types: Default::default(),
								node_graph_errors,
							});
							responses.add(NodeGraphMessage::SendGraph);

							return Err(format!("Node graph evaluation failed:\n{e}"));
						}
						Ok(result) => result,
					};

					responses.add(NodeGraphMessage::UpdateTypes {
						resolved_types: type_delta,
						node_graph_errors,
					});
					responses.add(NodeGraphMessage::SendGraph);
				}
				NodeGraphUpdate::NodeGraphUpdateMessage(NodeGraphUpdateMessage::ImaginateStatusUpdate) => {
					responses.add(DocumentMessage::PropertiesPanel(PropertiesPanelMessage::Refresh));
				}
			}
		}
		Ok(())
	}

	fn debug_render(render_object: impl GraphicElementRendered, transform: DAffine2, responses: &mut VecDeque<Message>) {
		// Setup rendering
		let mut render = SvgRender::new();
		let render_params = RenderParams::new(ViewMode::Normal, None, false, false, false);

		// Render SVG
		render_object.render_svg(&mut render, &render_params);

		// Concatenate the defs and the SVG into one string
		render.wrap_with_transform(transform, None);
		let svg = render.svg.to_svg_string();

		// Send to frontend
		responses.add(FrontendMessage::UpdateDocumentArtwork { svg });
	}

	fn process_node_graph_output(&mut self, node_graph_output: TaggedValue, transform: DAffine2, responses: &mut VecDeque<Message>) -> Result<(), String> {
		let mut render_output_metadata = RenderMetadata::default();
		match node_graph_output {
			TaggedValue::RenderOutput(render_output) => {
				match render_output.data {
					graphene_std::wasm_application_io::RenderOutputType::Svg(svg) => {
						// Send to frontend
						responses.add(FrontendMessage::UpdateDocumentArtwork { svg });
					}
					graphene_std::wasm_application_io::RenderOutputType::CanvasFrame(frame) => {
						let matrix = format_transform_matrix(frame.transform);
						let transform = if matrix.is_empty() { String::new() } else { format!(" transform=\"{}\"", matrix) };
						let svg = format!(
							r#"<svg><foreignObject width="{}" height="{}"{transform}><div data-canvas-placeholder="canvas{}"></div></foreignObject></svg>"#,
							frame.resolution.x, frame.resolution.y, frame.surface_id.0
						);
						responses.add(FrontendMessage::UpdateDocumentArtwork { svg });
					}
					_ => {
						return Err(format!("Invalid node graph output type: {:#?}", render_output.data));
					}
				}

				render_output_metadata = render_output.metadata;
			}
			TaggedValue::Bool(render_object) => Self::debug_render(render_object, transform, responses),
			TaggedValue::String(render_object) => Self::debug_render(render_object, transform, responses),
			TaggedValue::F64(render_object) => Self::debug_render(render_object, transform, responses),
			TaggedValue::DVec2(render_object) => Self::debug_render(render_object, transform, responses),
			TaggedValue::OptionalColor(render_object) => Self::debug_render(render_object, transform, responses),
			TaggedValue::VectorData(render_object) => Self::debug_render(render_object, transform, responses),
			TaggedValue::GraphicGroup(render_object) => Self::debug_render(render_object, transform, responses),
			TaggedValue::ImageFrame(render_object) => Self::debug_render(render_object, transform, responses),
			TaggedValue::Palette(render_object) => Self::debug_render(render_object, transform, responses),
			_ => {
				return Err(format!("Invalid node graph output type: {node_graph_output:#?}"));
			}
		};
		responses.add(Message::EndBuffer(render_output_metadata));
		responses.add(DocumentMessage::RenderScrollbars);
		responses.add(DocumentMessage::RenderRulers);
		responses.add(OverlaysMessage::Draw);
		Ok(())
	}
}

/// Stores all of the monitor nodes that have been attached to a graph
#[derive(Default)]
pub struct Instrumented {
	protonodes_by_name: HashMap<String, Vec<Vec<Vec<NodeId>>>>,
	protonodes_by_path: HashMap<Vec<NodeId>, Vec<Vec<NodeId>>>,
}

impl Instrumented {
	/// Adds montior nodes to the network
	fn add(&mut self, network: &mut NodeNetwork, path: &mut Vec<NodeId>) {
		// Required to do seperately to satiate the borrow checker.
		let mut monitor_nodes = Vec::new();
		for (id, node) in network.nodes.iter_mut() {
			// Recursively instrument
			if let DocumentNodeImplementation::Network(nested) = &mut node.implementation {
				path.push(*id);
				self.add(nested, path);
				path.pop();
			}
			let mut monitor_node_ids = Vec::with_capacity(node.inputs.len());
			for input in &mut node.inputs {
				let node_id = NodeId::new();
				let old_input = std::mem::replace(input, NodeInput::node(node_id, 0));
				monitor_nodes.push((old_input, node_id));
				path.push(node_id);
				monitor_node_ids.push(path.clone());
				path.pop();
			}
			if let DocumentNodeImplementation::ProtoNode(identifier) = &mut node.implementation {
				path.push(*id);
				self.protonodes_by_name.entry(identifier.name.to_string()).or_default().push(monitor_node_ids.clone());
				self.protonodes_by_path.insert(path.clone(), monitor_node_ids);
				path.pop();
			}
		}
		for (input, monitor_id) in monitor_nodes {
			let monitor_node = DocumentNode {
				inputs: vec![input],
				implementation: DocumentNodeImplementation::proto("graphene_core::memo::MonitorNode"),
				manual_composition: Some(graph_craft::generic!(T)),
				skip_deduplication: true,
				..Default::default()
			};
			network.nodes.insert(monitor_id, monitor_node);
		}
	}

	/// Instrument a graph and return a new [Instrumented] state.
	pub fn new(network: &mut NodeNetwork) -> Self {
		let mut instrumented = Self::default();
		instrumented.add(network, &mut Vec::new());
		instrumented
	}

	fn downcast<Input: graphene_std::NodeInputDecleration>(dynamic: Arc<dyn std::any::Any + Send + Sync>) -> Option<Input::Result>
	where
		Input::Result: Send + Sync + Clone + 'static,
	{
		// This is quite inflexible since it only allows the footprint as inputs.
		if let Some(x) = dynamic.downcast_ref::<IORecord<(), Input::Result>>() {
			Some(x.output.clone())
		} else if let Some(x) = dynamic.downcast_ref::<IORecord<Footprint, Input::Result>>() {
			Some(x.output.clone())
		} else if let Some(x) = dynamic.downcast_ref::<IORecord<Context, Input::Result>>() {
			Some(x.output.clone())
		} else {
			panic!("cannot downcast type for introspection");
		}
	}

	/// Grab all of the values of the input every time it occurs in the graph.
	pub fn grab_all_input<'a, Input: graphene_std::NodeInputDecleration + 'a>(&'a self, runtime: &'a NodeRuntime) -> impl Iterator<Item = Input::Result> + 'a
	where
		Input::Result: Send + Sync + Clone + 'static,
	{
		self.protonodes_by_name
			.get(Input::identifier())
			.map_or([].as_slice(), |x| x.as_slice())
			.iter()
			.filter_map(|inputs| inputs.get(Input::INDEX))
			.filter_map(|input_monitor_node| runtime.executor.introspect(input_monitor_node).ok())
			.filter_map(Instrumented::downcast::<Input>)
	}

	pub fn grab_protonode_input<Input: graphene_std::NodeInputDecleration>(&self, path: &Vec<NodeId>, runtime: &NodeRuntime) -> Option<Input::Result>
	where
		Input::Result: Send + Sync + Clone + 'static,
	{
		let input_monitor_node = self.protonodes_by_path.get(path)?.get(Input::INDEX)?;

		let dynamic = runtime.executor.introspect(input_monitor_node).ok()?;

		Self::downcast::<Input>(dynamic)
	}

	pub fn grab_input_from_layer<Input: graphene_std::NodeInputDecleration>(&self, layer: LayerNodeIdentifier, network_interface: &NodeNetworkInterface, runtime: &NodeRuntime) -> Option<Input::Result>
	where
		Input::Result: Send + Sync + Clone + 'static,
	{
		let node_graph_layer = NodeGraphLayer::new(layer, network_interface);
		let node = node_graph_layer.upstream_node_id_from_protonode(Input::identifier())?;
		self.grab_protonode_input::<Input>(&vec![node], runtime)
	}
}
