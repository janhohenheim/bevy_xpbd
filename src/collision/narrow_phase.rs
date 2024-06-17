//! Computes contacts between entities.
//!
//! See [`NarrowPhasePlugin`].

use std::marker::PhantomData;

use crate::{
    dynamics::solver::{
        contact::ContactConstraint, ContactConstraints, ContactSoftnessCoefficients, SolverConfig,
    },
    prelude::*,
};
#[cfg(feature = "parallel")]
use bevy::tasks::{ComputeTaskPool, ParallelSlice};
use bevy::{
    ecs::{
        schedule::{ExecutorKind, LogLevel, ScheduleBuildSettings},
        system::SystemParam,
    },
    prelude::*,
};

/// Computes contacts between entities and generates contact constraints for them.
///
/// Collisions are only checked between entities contained in [`BroadCollisionPairs`],
/// which is handled by the [`BroadPhasePlugin`].
///
/// The results of the narrow phase are added into [`Collisions`].
/// A [`ContactConstraint`] is generated for each contact manifold
/// and added to the [`ContactConstraints`] resource.
///
/// The plugin takes a collider type. This should be [`Collider`] for
/// the vast majority of applications, but for custom collisión backends
/// you may use any collider that implements the [`AnyCollider`] trait.
pub struct NarrowPhasePlugin<C: AnyCollider> {
    _phantom: PhantomData<C>,
}

impl<C: AnyCollider> Default for NarrowPhasePlugin<C> {
    fn default() -> Self {
        Self {
            _phantom: PhantomData,
        }
    }
}

impl<C: AnyCollider> Plugin for NarrowPhasePlugin<C> {
    fn build(&self, app: &mut App) {
        // For some systems, we only want one instance, even if there are multiple
        // NarrowPhasePlugin instances with different collider types.
        let is_first_instance = !app.world().is_resource_added::<NarrowPhaseInitialized>();

        app.init_resource::<NarrowPhaseInitialized>()
            .init_resource::<NarrowPhaseConfig>()
            .init_resource::<Collisions>()
            .register_type::<NarrowPhaseConfig>();

        app.configure_sets(
            PhysicsSchedule,
            (
                NarrowPhaseSet::First,
                NarrowPhaseSet::CollectCollisions,
                NarrowPhaseSet::PostProcess,
                NarrowPhaseSet::Last,
            )
                .chain()
                .in_set(PhysicsStepSet::NarrowPhase),
        );

        // Set up the PostProcessCollisions schedule for user-defined systems
        // that filter and modify collisions.
        app.edit_schedule(PostProcessCollisions, |schedule| {
            schedule
                .set_executor_kind(ExecutorKind::SingleThreaded)
                .set_build_settings(ScheduleBuildSettings {
                    ambiguity_detection: LogLevel::Error,
                    ..default()
                });
        });

        let physics_schedule = app
            .get_schedule_mut(PhysicsSchedule)
            .expect("add PhysicsSchedule first");

        // Manage collision states like `during_current_frame` and remove old contacts.
        // Only one narrow phase instance should do this.
        // TODO: It would be nice not to have collision state logic in the narrow phase.
        if is_first_instance {
            physics_schedule.add_systems(
                (
                    // Reset collision states.
                    reset_collision_states
                        .after(NarrowPhaseSet::First)
                        .before(NarrowPhaseSet::CollectCollisions),
                    // Remove ended collisions after contact reporting
                    remove_ended_collisions
                        .after(PhysicsStepSet::ReportContacts)
                        .before(PhysicsStepSet::Sleeping),
                )
                    .chain(),
            );
        }

        // Collect contacts into `Collisions`.
        physics_schedule.add_systems(
            collect_collisions::<C>
                .in_set(NarrowPhaseSet::CollectCollisions)
                // Allowing ambiguities is required so that it's possible
                // to have multiple collision backends at the same time.
                .ambiguous_with_all(),
        );

        if is_first_instance {
            #[cfg(debug_assertions)]
            physics_schedule.add_systems(
                log_overlap_at_spawn
                    .in_set(NarrowPhaseSet::PostProcess)
                    .before(run_post_process_collisions_schedule),
            );
            physics_schedule.add_systems(
                run_post_process_collisions_schedule.in_set(NarrowPhaseSet::PostProcess),
            );
        }
    }
}

#[derive(Resource, Default)]
struct NarrowPhaseInitialized;

/// A resource for configuring the [narrow phase](NarrowPhasePlugin).
#[derive(Resource, Reflect, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serialize", derive(serde::Serialize, serde::Deserialize))]
#[reflect(Resource)]
pub struct NarrowPhaseConfig {
    /// The default maximum [speculative margin](SpeculativeMargin) used for
    /// [speculative collisions](ccd#speculative-collision). This can be overridden
    /// for individual entities with the [`SpeculativeMargin`] component.
    ///
    /// This is implicitly scaled by the [`PhysicsLengthUnit`].
    ///
    /// Default: `MAX` (unbounded)
    pub default_speculative_margin: Scalar,

    /// A contact tolerance for detecting collisions even for
    /// slightly separated objects. This helps prevent numerical
    /// issues and missed collisions for resting contacts.
    ///
    /// This is implicitly scaled by the [`PhysicsLengthUnit`].
    ///
    /// Default: `0.005`
    pub contact_tolerance: Scalar,
}

impl Default for NarrowPhaseConfig {
    fn default() -> Self {
        Self {
            default_speculative_margin: Scalar::MAX,
            contact_tolerance: 0.005,
        }
    }
}

/// System sets for systems running in [`SubstepSet::NarrowPhase`].
#[derive(SystemSet, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NarrowPhaseSet {
    /// Runs at the start of the narrow phase. Empty by default.
    First,
    /// Computes contacts between entities and adds them to the [`Collisions`] resource.
    CollectCollisions,
    /// Responsible for running the [`PostProcessCollisions`] schedule to allow user-defined systems
    /// to filter and modify collisions.
    ///
    /// If you want to modify or remove collisions after [`NarrowPhaseSet::CollectCollisions`], you can
    /// add custom systems to this set, or to [`PostProcessCollisions`].
    PostProcess,
    /// Runs at the end of the narrow phase. Empty by default.
    Last,
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::type_complexity)]
fn collect_collisions<C: AnyCollider>(
    mut narrow_phase: NarrowPhase<C>,
    mut constraints: ResMut<ContactConstraints>,
    broad_collision_pairs: Res<BroadCollisionPairs>,
    solver_config: Res<SolverConfig>,
    contact_softness: Res<ContactSoftnessCoefficients>,
    time: Res<Time>,
) {
    let warm_start = solver_config.warm_start_coefficient > 0.0;

    narrow_phase.update(
        &broad_collision_pairs,
        &mut constraints,
        *contact_softness,
        warm_start,
        time.delta_seconds_adjusted(),
    );
}

/// A system parameter for managing the narrow phase.
///
/// The narrow phase computes contacts for each intersection pair
/// in [`BroadCollisionPairs`], adds them to the [`Collisions`] resource,
/// and generates [`ContactConstraints`] for the contacts.
#[derive(SystemParam)]
pub struct NarrowPhase<'w, 's, C: AnyCollider> {
    parallel_commands: ParallelCommands<'w, 's>,
    collider_query: Query<'w, 's, ColliderQuery<C>>,
    body_query: Query<'w, 's, (RigidBodyQueryReadOnly, Option<&'static SpeculativeMargin>)>,
    /// Contacts found by the narrow phase.
    pub collisions: ResMut<'w, Collisions>,
    /// Configuration options for the narrow phase.
    pub config: Res<'w, NarrowPhaseConfig>,
    length_unit: Res<'w, PhysicsLengthUnit>,
    // These are scaled by the length unit.
    default_speculative_margin: Local<'s, Scalar>,
    contact_tolerance: Local<'s, Scalar>,
}

impl<'w, 's, C: AnyCollider> NarrowPhase<'w, 's, C> {
    /// Updates the narrow phase by computing [`Contacts`] based on [`BroadCollisionPairs`],
    /// adding them to [`Collisions`], and generating [`ContactConstraint`](solver::contact::ContactConstraint)s
    /// for the contacts. The constraints are added to the given `constraints` vector.
    ///
    /// If `warm_start` is `true`, the current contacts will be matched with the previous contacts
    /// based on feature IDs or contact positions, and the constraints will be initialized with
    /// the contact impulses from the previous frame. This can help the solver resolve overlap
    /// and stabilize much faster.
    fn update(
        &mut self,
        broad_collision_pairs: &[(Entity, Entity)],
        constraints: &mut Vec<ContactConstraint>,
        contact_softness: ContactSoftnessCoefficients,
        warm_start: bool,
        delta_secs: Scalar,
    ) {
        // TODO: These scaled versions could be in their own resource
        //       and updated just before physics every frame.
        // Cache default margins scaled by the length unit.
        if self.config.is_changed() {
            *self.default_speculative_margin =
                self.length_unit.0 * self.config.default_speculative_margin;
            *self.contact_tolerance = self.length_unit.0 * self.config.contact_tolerance;
        }

        // Clear contact constraints.
        constraints.clear();

        #[cfg(feature = "parallel")]
        {
            // TODO: Verify if `par_splat_map` is deterministic. If not, sort the constraints (and collisions).
            broad_collision_pairs
                .iter()
                .par_splat_map(ComputeTaskPool::get(), None, |_i, chunks| {
                    let mut new_collisions = Vec::<Contacts>::with_capacity(chunks.len());
                    let mut new_constraints = Vec::<ContactConstraint>::with_capacity(chunks.len());

                    // Compute contacts for this intersection pair and generate
                    // contact constraints for them.
                    for &(entity1, entity2) in chunks {
                        if let Some(contacts) = self.handle_pair(
                            entity1,
                            entity2,
                            &mut new_constraints,
                            contact_softness,
                            warm_start,
                            delta_secs,
                        ) {
                            new_collisions.push(contacts);
                        }
                    }

                    (new_collisions, new_constraints)
                })
                .into_iter()
                .for_each(|(new_collisions, new_constraints)| {
                    // Add the collisions and constraints from each chunk.
                    self.collisions.extend(new_collisions);
                    constraints.extend(new_constraints);
                });
        }
        #[cfg(not(feature = "parallel"))]
        {
            // Compute contacts for this intersection pair and generate
            // contact constraints for them.
            for &(entity1, entity2) in broad_collision_pairs {
                if let Some(contacts) = self.handle_pair(
                    entity1,
                    entity2,
                    &mut constraints.0,
                    contact_softness,
                    warm_start,
                    delta_secs,
                ) {
                    self.collisions.insert_collision_pair(contacts);
                }
            }
        }
    }

    /// Returns the [`Contacts`] between `entity1` and `entity2` if they are intersecting,
    /// and generates [`ContactConstraint`]s for them, adding them to `constraints`.
    ///
    /// If `warm_start` is `true`, the current contacts will be matched with the previous contacts
    /// based on feature IDs or contact positions, and the constraints will be initialized with
    /// the contact impulses from the previous frame. This can help the solver resolve overlap
    /// and stabilize much faster.
    #[allow(clippy::too_many_arguments)]
    pub fn handle_pair(
        &self,
        entity1: Entity,
        entity2: Entity,
        constraints: &mut Vec<ContactConstraint>,
        contact_softness: ContactSoftnessCoefficients,
        warm_start: bool,
        delta_secs: Scalar,
    ) -> Option<Contacts> {
        let Ok([collider1, collider2]) = self.collider_query.get_many([entity1, entity2]) else {
            return None;
        };

        let body1_bundle = collider1
            .parent
            .and_then(|p| self.body_query.get(p.get()).ok());
        let body2_bundle = collider2
            .parent
            .and_then(|p| self.body_query.get(p.get()).ok());

        // The rigid body's collision margin and speculative margin will be used
        // if the collider doesn't have them specified.
        let (mut lin_vel1, rb_speculative_margin1) = body1_bundle
            .as_ref()
            .map_or((Vector::ZERO, None), |(body, speculative_margin)| {
                (body.linear_velocity.0, *speculative_margin)
            });
        let (mut lin_vel2, rb_speculative_margin2) = body2_bundle
            .as_ref()
            .map_or((Vector::ZERO, None), |(body, speculative_margin)| {
                (body.linear_velocity.0, *speculative_margin)
            });

        // Use the collider's own speculative margin if specified, and fall back to the body's
        // speculative margin.
        //
        // The speculative margin is used to predict contacts that might happen during the frame.
        // This is used for speculative collision. See the CCD and `SpeculativeMargin` documentation
        // for more details.
        let speculative_margin1 = collider1
            .speculative_margin
            .map_or(rb_speculative_margin1.map(|margin| margin.0), |margin| {
                Some(margin.0)
            });
        let speculative_margin2 = collider2
            .speculative_margin
            .map_or(rb_speculative_margin2.map(|margin| margin.0), |margin| {
                Some(margin.0)
            });

        // Compute the effective speculative margin, clamping it based on velocities and the maximum bound.
        let effective_speculative_margin = {
            let speculative_margin1 =
                speculative_margin1.unwrap_or(*self.default_speculative_margin);
            let speculative_margin2 =
                speculative_margin2.unwrap_or(*self.default_speculative_margin);
            let inv_delta_secs = delta_secs.recip();

            // Clamp velocities to the maximum speculative margins.
            if speculative_margin1 < Scalar::MAX {
                lin_vel1 = lin_vel1.clamp_length_max(speculative_margin1 * inv_delta_secs);
            }
            if speculative_margin2 < Scalar::MAX {
                lin_vel2 = lin_vel2.clamp_length_max(speculative_margin2 * inv_delta_secs);
            }

            // TODO: Check if AABBs intersect?

            // Compute the effective margin based on how much the bodies
            // are expected to move relative to each other.
            delta_secs * (lin_vel1 - lin_vel2).length()
        };

        // The maximum distance at which contacts are detected.
        // At least as large as the contact tolerance.
        let max_contact_distance = effective_speculative_margin.max(*self.contact_tolerance);

        let contacts = self.compute_contacts(
            &collider1,
            &collider2,
            max_contact_distance,
            // Only match contacts if warm starting is enabled.
            warm_start,
        )?;

        if let (Some(body1), Some(body2)) = (
            body1_bundle.map(|(body, _)| body),
            body2_bundle.map(|(body, _)| body),
        ) {
            // At least one of the bodies must be dynamic for contact constraints
            // to be generated.
            if !body1.rb.is_dynamic() && !body2.rb.is_dynamic() {
                return Some(contacts);
            }

            // Generate contact constraints for the computed contacts
            // and add them to `constraints`.
            self.generate_constraints(
                &contacts,
                constraints,
                &body1,
                &body2,
                &collider1,
                &collider2,
                contact_softness,
                warm_start,
                delta_secs,
            );
        }

        Some(contacts)
    }

    /// Computes contacts between `collider1` and `collider2`.
    /// Returns `None` if no contacts are found.
    ///
    /// The given `max_distance` determines the maximum distance for a contact
    /// to be detected. A value greater than zero means that contacts are generated
    /// based on the closest points even if the shapes are separated.
    ///
    /// If `match_contacts` is `true`, the current contacts will be matched with the previous contacts
    /// based on feature IDs or contact positions, and the contact impulses from the previous frame
    /// will be copied over for the new contacts. Using these impulses as the initial guess is referred to
    /// as *warm starting*, and it can help the contact solver resolve overlap and stabilize much faster.
    #[allow(clippy::type_complexity, clippy::too_many_arguments)]
    fn compute_contacts(
        &self,
        collider1: &ColliderQueryItem<C>,
        collider2: &ColliderQueryItem<C>,
        max_distance: Scalar,
        match_contacts: bool,
    ) -> Option<Contacts> {
        let position1 = collider1.current_position();
        let position2 = collider2.current_position();

        // TODO: It'd be good to persist the manifolds and let Parry match contacts.
        //       This isn't currently done because it requires using Parry's contact manifold type.
        // Compute the contact manifolds using the effective speculative margin.
        let mut manifolds = collider1.shape.contact_manifolds(
            collider2.shape,
            position1,
            *collider1.rotation,
            position2,
            *collider2.rotation,
            max_distance,
        );

        // Get the previous contacts if there are any.
        let previous_contacts = self
            .collisions
            .get_internal()
            .get(&(collider1.entity, collider2.entity))
            .or(self
                .collisions
                .get_internal()
                .get(&(collider2.entity, collider1.entity)));

        let mut total_normal_impulse = 0.0;
        let mut total_tangent_impulse = default();

        // Match contacts and copy previous contact impulses for warm starting the solver.
        // TODO: This condition is pretty arbitrary, mainly to skip dense trimeshes.
        //       If we let Parry handle contact matching, this wouldn't be needed.
        if manifolds.len() <= 4 && match_contacts {
            if let Some(previous_contacts) = previous_contacts {
                // TODO: Cache this?
                let distance_threshold = 0.1 * self.length_unit.0;

                for manifold in manifolds.iter_mut() {
                    for previous_manifold in previous_contacts.manifolds.iter() {
                        manifold.match_contacts(&previous_manifold.contacts, distance_threshold);

                        // Add contact impulses to total impulses.
                        for contact in manifold.contacts.iter() {
                            total_normal_impulse += contact.normal_impulse;
                            total_tangent_impulse += contact.tangent_impulse;
                        }
                    }
                }
            }
        }

        let contacts = Contacts {
            entity1: collider1.entity,
            entity2: collider2.entity,
            body_entity1: collider1.parent.map(|p| p.get()),
            body_entity2: collider2.parent.map(|p| p.get()),
            during_current_frame: true,
            during_previous_frame: previous_contacts.map_or(false, |c| c.during_previous_frame),
            manifolds,
            is_sensor: collider1.is_sensor
                || collider2.is_sensor
                || !collider1.is_rb
                || !collider2.is_rb,
            total_normal_impulse,
            total_tangent_impulse,
        };

        if !contacts.manifolds.is_empty() {
            return Some(contacts);
        }

        None
    }

    /// Generates [`ContactConstraint`]s for the given bodies and their corresponding colliders
    /// based on the given `contacts`. The constraints are added to the `constraints` vector.
    ///
    /// The `collision_margin` can be used to add artificial thickness to the colliders,
    /// which can improve performance and stability in some cases. See [`CollisionMargin`]
    /// for more details.
    ///
    /// The `contact_softness` is used to tune the damping and stiffness of the contact constraints.
    ///
    /// If `warm_start` is `true`, the constraints will be initialized with the impulses
    /// stored in the contacts from the previous frame. This can help the solver resolve overlap
    /// and stabilize much faster.
    #[allow(clippy::too_many_arguments)]
    pub fn generate_constraints(
        &self,
        contacts: &Contacts,
        constraints: &mut Vec<ContactConstraint>,
        body1: &RigidBodyQueryReadOnlyItem,
        body2: &RigidBodyQueryReadOnlyItem,
        collider1: &ColliderQueryItem<C>,
        collider2: &ColliderQueryItem<C>,
        contact_softness: ContactSoftnessCoefficients,
        warm_start: bool,
        delta_secs: Scalar,
    ) {
        let inactive1 = body1.rb.is_static() || body1.is_sleeping;
        let inactive2 = body2.rb.is_static() || body2.is_sleeping;

        // No collision response if both bodies are static or sleeping
        // or if either of the colliders is a sensor collider.
        if (inactive1 && inactive2)
            || (collider1.is_sensor || body1.is_sensor)
            || (collider2.is_sensor || body2.is_sensor)
        {
            return;
        }

        // When an active body collides with a sleeping body, wake up the sleeping body.
        self.parallel_commands.command_scope(|mut commands| {
            if body1.is_sleeping {
                commands.entity(body1.entity).remove::<Sleeping>();
            } else if body2.is_sleeping {
                commands.entity(body2.entity).remove::<Sleeping>();
            }
        });

        // Get combined friction and restitution coefficients of the colliders
        // or the bodies they are attached to.
        let friction = collider1
            .friction
            .unwrap_or(body1.friction)
            .combine(*collider2.friction.unwrap_or(body2.friction));
        let restitution = collider1
            .restitution
            .unwrap_or(body1.restitution)
            .combine(*collider2.restitution.unwrap_or(body2.restitution));

        let contact_softness = if !body1.rb.is_dynamic() || !body2.rb.is_dynamic() {
            contact_softness.non_dynamic
        } else {
            contact_softness.dynamic
        };

        // Generate contact constraints for each contact.
        for (i, contact_manifold) in contacts.manifolds.iter().enumerate() {
            let constraint = ContactConstraint::generate(
                i,
                contact_manifold,
                body1,
                body2,
                collider1.entity,
                collider2.entity,
                collider1.transform.copied(),
                collider2.transform.copied(),
                // TODO: Shouldn't this be the effective speculative margin?
                *self.default_speculative_margin,
                friction,
                restitution,
                contact_softness,
                warm_start,
                delta_secs,
            );

            if !constraint.points.is_empty() {
                constraints.push(constraint);
            }
        }
    }
}

#[cfg(debug_assertions)]
fn log_overlap_at_spawn(
    collisions: Res<Collisions>,
    added_bodies: Query<(Ref<RigidBody>, Option<&Name>, &Position)>,
) {
    for contacts in collisions.get_internal().values() {
        let Ok([(rb1, name1, position1), (rb2, name2, position2)]) = added_bodies.get_many([
            contacts.body_entity1.unwrap_or(contacts.entity1),
            contacts.body_entity2.unwrap_or(contacts.entity2),
        ]) else {
            continue;
        };

        if rb1.is_added() || rb2.is_added() {
            // If the RigidBody entity has a name, use that for debug.
            let debug_id1 = match name1 {
                Some(n) => format!("{:?} ({n})", contacts.entity1),
                None => format!("{:?}", contacts.entity1),
            };
            let debug_id2 = match name2 {
                Some(n) => format!("{:?} ({n})", contacts.entity2),
                None => format!("{:?}", contacts.entity2),
            };
            warn!(
                "{} and {} are overlapping at spawn, which can result in explosive behavior.",
                debug_id1, debug_id2,
            );
            debug!("{} is at {}", debug_id1, position1.0);
            debug!("{} is at {}", debug_id2, position2.0);
        }
    }
}

fn remove_ended_collisions(mut collisions: ResMut<Collisions>) {
    collisions.retain(|contacts| contacts.during_current_frame);
}

// TODO: The collision state handling feels a bit confusing and error-prone.
//       Ideally, the narrow phase wouldn't need to handle it at all, or it would at least be simpler.
/// Resets collision states like `during_current_frame` and `during_previous_frame`.
pub fn reset_collision_states(
    mut collisions: ResMut<Collisions>,
    query: Query<(Option<&RigidBody>, Has<Sleeping>)>,
) {
    for contacts in collisions.get_internal_mut().values_mut() {
        contacts.total_normal_impulse = 0.0;
        contacts.total_tangent_impulse = default();

        if let Ok([(rb1, sleeping1), (rb2, sleeping2)]) =
            query.get_many([contacts.entity1, contacts.entity2])
        {
            let active1 = !rb1.map_or(false, |rb| rb.is_static()) && !sleeping1;
            let active2 = !rb2.map_or(false, |rb| rb.is_static()) && !sleeping2;

            // Reset collision states if either of the bodies is active (not static or sleeping)
            // Otherwise, the bodies are still in contact.
            if active1 || active2 {
                contacts.during_previous_frame = true;
                contacts.during_current_frame = false;
            } else {
                contacts.during_previous_frame = true;
                contacts.during_current_frame = true;
            }
        } else {
            contacts.during_current_frame = false;
        }
    }
}

/// Runs the [`PostProcessCollisions`] schedule.
fn run_post_process_collisions_schedule(world: &mut World) {
    trace!("running PostProcessCollisions");
    world.run_schedule(PostProcessCollisions);
}
