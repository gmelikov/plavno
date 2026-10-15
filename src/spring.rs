/// Exact critically damped spring step for a constant target.
pub fn advance(position: f32, velocity: f32, target: f32, omega: f32, dt: f32) -> (f32, f32) {
    let offset = position - target;
    let inertia = velocity + omega * offset;
    let decay = (-omega * dt).exp();
    (
        target + (offset + inertia * dt) * decay,
        (velocity - omega * inertia * dt) * decay,
    )
}
