//! Server item and block displays (`ClientboundAddEntityPacket` types
//! `minecraft:item_display` and `minecraft:block_display`): the `Display`
//! transform and its interpolation, with an item model or a baked block model
//! submitted under it.
use super::Game;
use horizon_assets::item_model::DisplayContext;
use horizon_client::items::ItemStack;
use horizon_client::mesher::Vertex;
use horizon_client::text_display::TextDisplayData;
use horizon_protocol::entity_data::Value;
use horizon_world::registry::BlockStateId;

pub(super) enum DisplayKind {
    Item {
        stack: Option<ItemStack>,
        context: DisplayContext,
    },
    Block {
        state: BlockStateId,
    },
}

/// `Display`, an `ItemDisplay` or a `BlockDisplay` a server sent.
pub(super) struct RemoteDisplay {
    pub kind: DisplayKind,
    /// `Display`'s synced data (ids 8 to 22) as the last packet left it.
    data: TextDisplayData,
    from: TextDisplayData,
    shown: TextDisplayData,
    position: glam::DVec3,
    previous: glam::DVec3,
    target: glam::DVec3,
    rotation: [f32; 2],
    old_rotation: [f32; 2],
    target_rotation: [f32; 2],
    steps: i32,
    age: i64,
    start: i64,
    duration: i32,
    dirty: bool,
    start_dirty: bool,
    duration_dirty: bool,
    initialized: bool,
    /// `Display.lastProgress`: the progress the last frame drew, where a new
    /// interpolation starts from.
    last_progress: f32,
}

/// `ItemDisplay.RENDER_STATE_IDS` and `BlockDisplay.RENDER_STATE_IDS`: the
/// `Display` transformation, the billboard, brightness and shadows, plus the
/// item or the block state.
fn renders(id: u8) -> bool {
    matches!(id, 11..=16 | 18 | 19 | 23 | 24)
}

impl RemoteDisplay {
    pub fn new(position: [f64; 3], yaw: f32, kind: DisplayKind) -> Self {
        Self {
            kind,
            data: Default::default(),
            from: Default::default(),
            shown: Default::default(),
            position: position.into(),
            previous: position.into(),
            target: position.into(),
            rotation: [yaw, 0.0],
            old_rotation: [yaw, 0.0],
            target_rotation: [yaw, 0.0],
            steps: 0,
            age: 0,
            start: 0,
            duration: 0,
            dirty: true,
            start_dirty: false,
            duration_dirty: false,
            initialized: false,
            last_progress: 0.0,
        }
    }
    fn progress(&self, partial: f32) -> f32 {
        if self.duration <= 0 {
            1.0
        } else {
            (((self.age - self.start) as f32 + partial) / self.duration as f32).clamp(0.0, 1.0)
        }
    }
    /// `Display.onSyncedDataUpdated` for the shared data, and the item display
    /// context or block state; the item stack is applied by the caller, which
    /// resolves it through the item registry.
    pub fn metadata(&mut self, values: &[(u8, i32, Value)]) {
        self.start_dirty |= values.iter().any(|v| v.0 == 8);
        self.duration_dirty |= values.iter().any(|v| v.0 == 9);
        self.dirty |= values.iter().any(|v| renders(v.0));
        for (index, _, value) in values {
            match (index, value) {
                // BlockDisplay.DATA_BLOCK_STATE_ID.
                (23, Value::Id(state)) => {
                    if let DisplayKind::Block { state: block } = &mut self.kind {
                        *block = BlockStateId((*state).max(0) as u32);
                    }
                }
                // ItemDisplay.DATA_ITEM_DISPLAY_CONTEXT, an `ItemDisplayContext` id.
                (24, Value::Byte(context)) => {
                    if let DisplayKind::Item {
                        context: current, ..
                    } = &mut self.kind
                    {
                        *current = DisplayContext::ALL
                            .get(*context as usize)
                            .copied()
                            .unwrap_or(DisplayContext::None);
                    }
                }
                _ => {}
            }
        }
        self.data.apply(values);
    }
    pub fn set_item(&mut self, stack: Option<ItemStack>) {
        if let DisplayKind::Item { stack: slot, .. } = &mut self.kind {
            *slot = stack;
        }
    }
    fn tick(&mut self) {
        self.age += 1;
        self.previous = self.position;
        self.old_rotation = self.rotation;
        if self.steps > 0 {
            self.position += (self.target - self.position) / f64::from(self.steps);
            for i in 0..2 {
                self.rotation[i] +=
                    super::text_displays::angle(self.target_rotation[i] - self.rotation[i])
                        / self.steps as f32;
            }
            self.steps -= 1;
        }
        if self.start_dirty {
            self.start = self.age + i64::from(self.data.interpolation_delay);
        }
        if self.duration_dirty {
            self.duration = self.data.interpolation_duration;
        }
        if self.dirty {
            self.from = if self.initialized && self.duration > 0 {
                self.shown.blend(&self.from, self.last_progress)
            } else {
                self.data.clone()
            };
            self.shown = self.data.clone();
            self.initialized = true;
        }
        self.dirty = false;
        self.start_dirty = false;
        self.duration_dirty = false;
    }
    fn move_to(&mut self, position: [f64; 3], rotation: Option<(f32, f32)>) {
        self.target = position.into();
        if let Some(rotation) = rotation {
            self.target_rotation = [rotation.0, rotation.1];
        }
        self.steps = self.data.teleport_duration.clamp(0, 59);
        if self.steps == 0 {
            self.position = self.target;
            self.previous = self.target;
            self.rotation = self.target_rotation;
            self.old_rotation = self.rotation;
        }
    }
    /// `DisplayRenderer.submit`'s pose at `partial`: the camera-relative
    /// Display transformation and the data blended between the last two.
    fn pose(
        &mut self,
        camera: glam::DVec3,
        partial: f64,
        angles: [f32; 2],
        scale: f64,
    ) -> Option<(TextDisplayData, glam::Mat4, glam::DVec3)> {
        self.last_progress = self.progress(partial as f32);
        let data = self.shown.blend(&self.from, self.last_progress);
        let position = self.previous.lerp(self.position, partial);
        // `Display.shouldRenderAtSqrDistance` reads the live view range, not
        // the interpolated transform.
        let range = f64::from(self.data.view_range) * 64.0 * scale;
        if position.distance_squared(camera) >= range * range {
            return None;
        }
        let rotation = std::array::from_fn(|i| {
            self.old_rotation[i]
                + super::text_displays::angle(self.rotation[i] - self.old_rotation[i])
                    * partial as f32
        });
        let matrix = glam::Mat4::from_translation((position - camera).as_vec3())
            * data.matrix(rotation, angles);
        matrix.is_finite().then_some((data, matrix, position))
    }
}

impl Game<'_> {
    /// `ClientboundSetEntityDataPacket` for an item or block display.
    pub(super) fn remote_display_data(&mut self, id: i32, values: &[(u8, i32, Value)]) {
        // The stack is resolved through the item registry, which needs `&self`.
        let item = values
            .iter()
            .find_map(|(index, _, value)| match (index, value) {
                (23, Value::Item(stack)) => Some(self.to_stack(stack)),
                _ => None,
            });
        let Some(display) = self.displays.get_mut(&id) else {
            return;
        };
        if let Some(item) = item {
            display.set_item(item);
        }
        display.metadata(values);
    }
    pub(super) fn move_remote_display(
        &mut self,
        id: i32,
        position: [f64; 3],
        rotation: Option<(f32, f32)>,
    ) {
        if let Some(display) = self.displays.get_mut(&id) {
            display.move_to(position, rotation);
        }
    }
    pub(super) fn tick_displays(&mut self) {
        for display in self.displays.values_mut() {
            display.tick();
        }
    }
    /// `ItemDisplayRenderer.submitInner` and `BlockDisplayRenderer.submitInner`
    /// for every display the server sent.
    pub(super) fn display_geometry(
        &mut self,
        camera: glam::DVec3,
        partial: f64,
        out: &mut Vec<Vertex>,
    ) {
        // The item and block models take `&self`: iterate the displays out of
        // the game, as the falling blocks do in `tick_remote_projectiles`.
        let mut displays = std::mem::take(&mut self.displays);
        let front = self.camera_type == super::perspective::CameraType::ThirdPersonFront;
        let angles = if front {
            [self.yaw + 180.0, -self.pitch]
        } else {
            [self.yaw, self.pitch]
        };
        let scale = self.view_scale();
        self.mark_entity(out.len(), "minecraft:item_display");
        for (&id, display) in displays.iter_mut() {
            let Some((data, matrix, position)) = display.pose(camera, partial, angles, scale)
            else {
                continue;
            };
            let DisplayKind::Item {
                stack: Some(stack),
                context,
            } = &display.kind
            else {
                continue;
            };
            let block = [
                position.x.floor() as i32,
                position.y.floor() as i32,
                position.z.floor() as i32,
            ];
            let light = data.light(self.world.light(block[0], block[1], block[2]));
            let owner = super::item_draw::owner_at(
                [position.x, position.y, position.z],
                0.0,
                "minecraft:item_display",
            );
            let state = self.resolve_item(stack, *context, Some(&owner), id);
            if state.is_empty() {
                continue;
            }
            // ItemDisplayRenderer.submitInner turns the model to face the
            // display's forward direction before submitting the item.
            let matrix = matrix * glam::Mat4::from_rotation_y(std::f32::consts::PI);
            let start = out.len();
            self.submit_item_world(&state, matrix, light, out);
            self.cull_entity_vertices(out, start);
        }
        self.mark_entity(out.len(), "minecraft:block_display");
        for display in displays.values_mut() {
            let Some((data, matrix, position)) = display.pose(camera, partial, angles, scale)
            else {
                continue;
            };
            let DisplayKind::Block { state } = &display.kind else {
                continue;
            };
            let block = [
                position.x.floor() as i32,
                position.y.floor() as i32,
                position.z.floor() as i32,
            ];
            let light = data.light(self.world.light(block[0], block[1], block[2]));
            // BlockDisplayRenderer submits the model under the display's
            // transform alone: a block model is baked from 0 to 1, so its
            // minimum corner sits at the entity's position.
            let m = matrix;
            let seed = horizon_core::random::position_seed(horizon_core::position::BlockPos::new(
                block[0], block[1], block[2],
            ));
            let start = out.len();
            self.block_model_vertices(*state, seed, m, light, false, out);
            // The level's grass and foliage tint, as a falling block takes it.
            if let Some((_, tint)) = self.particle_sprite(*state, block) {
                let tint = horizon_client::mobs::render::abgr(tint);
                for vertex in &mut out[start..] {
                    vertex.color = horizon_client::hud_layout::multiply(vertex.color, tint);
                }
            }
            self.cull_entity_vertices(out, start);
        }
        self.displays = displays;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_sets_the_item_context_and_the_block_state() {
        let mut item = RemoteDisplay::new(
            [0.0; 3],
            0.0,
            DisplayKind::Item {
                stack: None,
                context: DisplayContext::None,
            },
        );
        item.metadata(&[
            (24, 0, Value::Byte(8)),
            (11, 0, Value::Floats(vec![1.0, 2.0, 3.0])),
            (15, 0, Value::Byte(3)),
        ]);
        match &item.kind {
            DisplayKind::Item { context, .. } => assert_eq!(*context, DisplayContext::Fixed),
            DisplayKind::Block { .. } => panic!("an item display changed kind"),
        }
        assert_eq!(item.data.translation, [1.0, 2.0, 3.0]);
        assert_eq!(item.data.billboard, 3);

        let mut block = RemoteDisplay::new(
            [0.0; 3],
            0.0,
            DisplayKind::Block {
                state: BlockStateId(0),
            },
        );
        block.metadata(&[(23, 0, Value::Id(42))]);
        match &block.kind {
            DisplayKind::Block { state } => assert_eq!(*state, BlockStateId(42)),
            DisplayKind::Item { .. } => panic!("a block display changed kind"),
        }
    }

    #[test]
    fn a_context_id_past_the_enum_is_none() {
        let mut item = RemoteDisplay::new(
            [0.0; 3],
            0.0,
            DisplayKind::Item {
                stack: None,
                context: DisplayContext::Fixed,
            },
        );
        item.metadata(&[(24, 0, Value::Byte(99))]);
        match &item.kind {
            DisplayKind::Item { context, .. } => assert_eq!(*context, DisplayContext::None),
            DisplayKind::Block { .. } => panic!("an item display changed kind"),
        }
    }
}
