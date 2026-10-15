use crate::choreography::ease;

#[derive(Clone, Copy, Debug, PartialEq)]
enum State {
    Steady,
    Catching { elapsed: f32, stepped: bool },
    Falling { elapsed: f32, side: f32 },
    Rising { elapsed: f32 },
}

pub struct Balance {
    pub angle: f32,
    velocity: f32,
    state: State,
    cooldown: f32,
    support_error: f32,
    unsupported: f32,
}

impl Default for Balance {
    fn default() -> Self {
        Self {
            angle: 0.0,
            velocity: 0.0,
            state: State::Steady,
            cooldown: 0.0,
            support_error: 0.0,
            unsupported: 0.0,
        }
    }
}

impl Balance {
    pub fn recovering(&self) -> bool {
        self.state != State::Steady
    }

    pub fn can_overbalance(&self) -> bool {
        self.cooldown == 0.0 && self.state == State::Steady
    }

    pub fn kick(&mut self, impulse: f32) {
        if matches!(self.state, State::Steady | State::Catching { .. }) {
            self.velocity = (self.velocity + impulse).clamp(-8.0, 8.0);
        }
    }

    pub fn observe_support(&mut self, error: f32, dt: f32) {
        self.support_error = error;
        if matches!(self.state, State::Falling { .. } | State::Rising { .. }) {
            self.unsupported = 0.0;
            return;
        }
        self.unsupported = if error.abs() > 0.12 {
            self.unsupported + dt
        } else {
            0.0
        };
        if self.unsupported > 0.4 && error.abs() > 0.3 {
            // The cooldown limits improvised accidents, not a real loss of support.
            self.state = State::Falling {
                elapsed: 0.0,
                side: error.signum(),
            };
            self.cooldown = 40.0;
        } else if self.unsupported > 0.18 && self.state == State::Steady {
            self.state = State::Catching {
                elapsed: 0.0,
                stepped: false,
            };
        }
    }

    pub fn advance(&mut self, dt: f32, grounded: bool) {
        self.cooldown = (self.cooldown - dt).max(0.0);
        match self.state {
            State::Steady | State::Catching { .. } => {
                // An underdamped torso carries momentum past the supporting foot.
                let steps = (dt * 240.0).ceil().max(1.0) as usize;
                let step = dt / steps as f32;
                for _ in 0..steps {
                    self.velocity += (-25.0 * self.angle - 5.2 * self.velocity) * step;
                    self.angle += self.velocity * step;
                }
                if grounded
                    && self.cooldown == 0.0
                    && self.angle.abs() + self.velocity.abs() * 0.1 > 0.64
                {
                    self.state = State::Falling {
                        elapsed: 0.0,
                        side: (self.angle + self.velocity * 0.1).signum(),
                    };
                    self.cooldown = 40.0;
                } else if self.state == State::Steady && grounded && self.angle.abs() > 0.18 {
                    self.state = State::Catching {
                        elapsed: 0.0,
                        stepped: false,
                    };
                } else if let State::Catching { elapsed, stepped } = self.state {
                    self.state = if grounded
                        && self.angle.abs() < 0.05
                        && self.velocity.abs() < 0.18
                        && (stepped || elapsed > 1.0)
                    {
                        State::Steady
                    } else {
                        State::Catching {
                            elapsed: elapsed + dt,
                            stepped,
                        }
                    };
                }
            }
            State::Falling { elapsed, side } => {
                self.follow(side * 1.25, 7.0, dt);
                self.state = if elapsed + dt >= 1.2 {
                    State::Rising { elapsed: 0.0 }
                } else {
                    State::Falling {
                        elapsed: elapsed + dt,
                        side,
                    }
                };
            }
            State::Rising { elapsed } => {
                self.follow(0.0, 3.8, dt);
                self.state = if elapsed + dt >= 2.2 && self.angle.abs() < 0.035 {
                    State::Steady
                } else {
                    State::Rising {
                        elapsed: elapsed + dt,
                    }
                };
            }
        }
    }

    fn follow(&mut self, target: f32, omega: f32, dt: f32) {
        (self.angle, self.velocity) =
            crate::spring::advance(self.angle, self.velocity, target, omega, dt);
    }

    pub fn catch_direction(&self) -> Option<f32> {
        match self.state {
            State::Catching { stepped: false, .. } => Some(if self.support_error.abs() > 0.12 {
                self.support_error.signum()
            } else {
                self.angle.signum()
            }),
            _ => None,
        }
    }

    pub fn step_started(&mut self) {
        if let State::Catching { elapsed, .. } = self.state {
            self.state = State::Catching {
                elapsed,
                stepped: true,
            };
        }
    }

    pub fn brace(&self) -> f32 {
        match self.state {
            State::Falling { elapsed, .. } => ease((elapsed / 0.65).min(1.0)),
            State::Rising { elapsed } => 1.0 - ease((elapsed / 2.2).min(1.0)),
            _ => 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_momentum_needs_a_step_and_cannot_hide_behind_the_fall_cooldown() {
        let mut balance = Balance::default();
        for _ in 0..30 {
            balance.advance(1.0 / 120.0, true);
            balance.observe_support(-0.2, 1.0 / 120.0);
        }
        assert_eq!(balance.catch_direction(), Some(-1.0));
        balance.step_started();
        for _ in 0..240 {
            balance.advance(1.0 / 120.0, true);
            balance.observe_support(0.0, 1.0 / 120.0);
        }
        assert!(!balance.recovering());
        balance.cooldown = 40.0;
        for _ in 0..60 {
            balance.advance(1.0 / 120.0, true);
            balance.observe_support(0.4, 1.0 / 120.0);
        }
        assert!(matches!(balance.state, State::Falling { side: 1.0, .. }));
        for _ in 0..720 {
            balance.advance(1.0 / 120.0, true);
            balance.observe_support(0.0, 1.0 / 120.0);
        }
        assert!(!balance.recovering());
    }

    #[test]
    fn momentum_calls_for_a_step_and_extreme_momentum_falls_then_recovers() {
        let mut balance = Balance::default();
        balance.kick(3.0);
        let mut caught = false;
        for _ in 0..360 {
            balance.advance(1.0 / 120.0, true);
            if balance.catch_direction().is_some() {
                caught = true;
                balance.step_started();
            }
            assert!(!matches!(balance.state, State::Falling { .. }));
        }
        assert!(caught && !balance.recovering());
        balance.kick(-8.0);
        let mut fell = false;
        let mut lowered = false;
        let mut previous = balance.angle;
        for _ in 0..720 {
            balance.advance(1.0 / 120.0, true);
            fell |= matches!(balance.state, State::Falling { .. });
            lowered |= balance.brace() > 0.9;
            assert!((previous - balance.angle).abs() < 0.08);
            assert!(balance.angle.abs() < 1.4);
            previous = balance.angle;
        }
        assert!(fell && lowered && !balance.recovering());
        assert!(!balance.can_overbalance(), "Missing fall cooldown");
    }

    #[test]
    fn ordinary_beats_stay_upright_and_airborne_motion_does_not_start_a_fall() {
        let mut balance = Balance::default();
        for frame in 0..7200 {
            if frame % 60 == 0 {
                balance.kick(if frame % 120 == 0 { 1.0 } else { -1.0 });
            }
            balance.advance(1.0 / 120.0, true);
            assert!(!balance.recovering());
        }
        balance.kick(8.0);
        for _ in 0..30 {
            balance.advance(1.0 / 120.0, false);
            assert!(!balance.recovering());
        }
    }
}
