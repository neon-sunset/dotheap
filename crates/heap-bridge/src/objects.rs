use crate::{
    models::*,
    snapshot::{ObjectInfo, Result, RootPath, Snapshot, StateMachineField},
    types::Index,
};

pub struct NativeObjectChunk {
    pub id: i32,
    pub label: String,
    pub size: u64,
    pub field: Option<String>,
}

pub struct NativeObjectMemory {
    pub chunks: Vec<NativeObjectChunk>,
    pub kept: u64,
}

pub struct DeadObject {
    pub referrers: Vec<Index>,
    pub path: Vec<Index>,
    pub path_names: Vec<Option<String>>,
    pub holds: u64,
}

pub struct ObjectDetail {
    pub info: ObjectInfo,
    pub type_name: String,
    pub method_table: u64,
    pub length: Option<Index>,
    pub fields: Vec<FieldValue>,
    pub preview: Option<TextPreview>,
    pub struct_elements: Option<Vec<StructElement>>,
    pub reference_elements: Option<Vec<ReferenceElement>>,
    pub path: RootPath,
    pub path_names: Vec<Option<String>>,
    pub root_slot: Option<String>,
    pub referrers: Vec<Index>,
    pub referrer_names: Vec<Option<String>>,
    pub sources: Vec<Index>,
    pub references: Vec<i32>,
    pub words: Vec<Word>,
    pub native: Option<NativeObjectMemory>,
    pub dead: Option<DeadObject>,
}

impl Snapshot {
    fn path_names(&self, objects: &[Index]) -> heap_core::Result<Vec<Option<String>>> {
        std::iter::once(Ok(None))
            .chain(
                objects
                    .windows(2)
                    .map(|p| self.core.reference_name(p[0], p[1])),
            )
            .take(objects.len())
            .collect()
    }
}

impl Snapshot {
    pub fn inspect_object(&self, object: Index) -> Result<ObjectDetail> {
        self.validate_object(object)?;
        let s = &self.core;
        let (source, path) = s.graph.path_to_root(object);
        let path = if s.graph.live[object] != 0 {
            path
        } else {
            Vec::new()
        };
        let referrers: Vec<_> = s
            .graph
            .referrer_rows
            .row(object)
            .iter()
            .take(300)
            .map(|&i| i as usize)
            .collect();
        let referrer_names = referrers
            .iter()
            .map(|&i| s.reference_name(i, object))
            .collect::<heap_core::Result<_>>()?;
        let native_graph = s.native_graph()?;
        let native = native_graph
            .owned_chunks
            .get(&object)
            .map(|chunks| NativeObjectMemory {
                chunks: chunks
                    .iter()
                    .take(100)
                    .map(|&c| NativeObjectChunk {
                        id: c,
                        label: native_graph
                            .class_name(c as usize)
                            .unwrap_or_else(|| native_graph.reach[c as usize].title().into()),
                        size: native_graph.chunks[c as usize].size,
                        field: native_graph.owner_field(object, c as usize, s),
                    })
                    .collect(),
                kept: native_graph
                    .native_retained
                    .get(&object)
                    .copied()
                    .unwrap_or(0),
            });
        let dead = if s.graph.live[object] == 0 {
            let g = s.garbage()?;
            let path = g.path_from_unreferenced(object);
            Some(DeadObject {
                referrers: g
                    .referrers
                    .row(object)
                    .iter()
                    .take(300)
                    .map(|&i| i as usize)
                    .collect(),
                path_names: self.path_names(&path)?,
                path,
                holds: g.retained[object],
            })
        } else {
            None
        };
        let struct_elements = s.struct_elements(object, 100);
        let reference_elements = if struct_elements.is_none() {
            s.reference_elements(object, 300)
        } else {
            None
        };
        let detail = ObjectDetail {
            info: self.object_info(object).expect("object was validated"),
            type_name: s.type_name(object),
            method_table: s.heap.method_table(object).address,
            length: s.component_count(object),
            fields: s.field_values(object),
            preview: s.text_preview(object, 8192),
            struct_elements,
            reference_elements,
            path_names: self.path_names(&path)?,
            root_slot: path
                .first()
                .map(|&i| s.root_slot_description(i))
                .transpose()?
                .flatten(),
            path: RootPath {
                source,
                objects: path,
            },
            referrers,
            referrer_names,
            sources: s
                .graph
                .sources_of
                .row(object)
                .iter()
                .map(|&i| i as usize)
                .collect(),
            references: s.graph.references(object).into_iter().take(300).collect(),
            words: s.words(object, 256),
            native,
            dead,
        };
        self.checked(Ok(detail))
    }

    pub fn field_values(&self, object: Index) -> Result<Vec<FieldValue>> {
        self.validate_object(object)?;
        self.checked(Ok(self.core.field_values(object)))
    }

    pub fn identifying_text(&self, object: Index) -> Result<Option<String>> {
        self.validate_object(object)?;
        self.checked(Ok(self.core.identifying_text(object)))
    }

    pub fn state_machine_fields(&self, object: Index) -> Result<Vec<StateMachineField>> {
        self.validate_object(object)?;
        self.checked(Ok(self
            .core
            .state_machine_fields(object)
            .into_iter()
            .map(|(value, retained_by)| StateMachineField { value, retained_by })
            .collect()))
    }

    pub fn reference_name(&self, parent: Index, child: Index) -> Result<Option<String>> {
        self.validate_object(parent)?;
        self.validate_object(child)?;
        self.checked(self.core.reference_name(parent, child))
    }

    pub fn stack_holder_text(&self, object: Index) -> Result<Option<String>> {
        self.validate_object(object)?;
        self.checked(self.core.stack_holder_text(object))
    }

    pub fn socket_text(&self, object: Index) -> Result<Option<String>> {
        self.validate_object(object)?;
        self.checked(Ok(self.core.socket_text(object)))
    }

    pub fn path_avoiding(&self, object: Index, types: Vec<String>) -> Result<Option<RootPath>> {
        self.validate_object(object)?;
        let avoid = self
            .core
            .type_stats
            .iter()
            .filter(|t| types.contains(&t.name))
            .map(|t| t.type_id)
            .collect();
        Ok(self
            .core
            .path_avoiding(object, &avoid)
            .map(|(source, objects)| RootPath { source, objects }))
    }
}
