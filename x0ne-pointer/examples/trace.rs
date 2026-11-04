//! Draws a path in the terminal.
//!
//! ```sh
//! cargo run -p x0ne-pointer --example trace
//! ```
//!
//! Used to see what the module produces, and above all the difference with
//! what the two drivers did before: a jump.

use x0ne_pointer::{duration_ms, path, Bounds, Profile, Step};

/// Renders the steps in a grid, `A` at the start, `B` at the end.
fn draw(steps: &[Step], from: (i32, i32), to: (i32, i32), w: usize, h: usize, b: Bounds) -> String {
    let mut grid = vec![vec![' '; w]; h];
    let map = |x: i32, y: i32| -> (usize, usize) {
        let cx = (x as f64 / b.width as f64 * (w - 1) as f64).round() as usize;
        let cy = (y as f64 / b.height as f64 * (h - 1) as f64).round() as usize;
        (cx.min(w - 1), cy.min(h - 1))
    };
    for s in steps {
        let (cx, cy) = map(s.x, s.y);
        grid[cy][cx] = '·';
    }
    let (ax, ay) = map(from.0, from.1);
    grid[ay][ax] = 'A';
    let (bx, by) = map(to.0, to.1);
    grid[by][bx] = 'B';
    grid.into_iter()
        .map(|r| format!("  │{}│", r.into_iter().collect::<String>()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn main() {
    let bounds = Bounds {
        width: 1920,
        height: 1080,
    };
    let (from, to) = ((80, 900), (1700, 160));
    let w = 62;
    let h = 15;
    let rule = format!("  ┌{}┐", "─".repeat(w));
    let bot = format!("  └{}┘", "─".repeat(w));

    for (title, p, seed) in [
        (
            "BEFORE — the jump, what the two drivers did",
            Profile::direct(),
            1,
        ),
        ("AFTER — default trajectory", Profile::default(), 1),
        (
            "the same path, seed 3: it curves the other way",
            Profile::default(),
            3,
        ),
        (
            "wide profile: pronounced arc, 6 px jitter",
            Profile {
                bow: 0.22,
                jitter_px: 6.0,
                px_per_step: 8.0,
                ..Profile::default()
            },
            3,
        ),
    ] {
        let steps = path(from, to, bounds, &p, seed);
        println!("\n  {title}");
        println!(
            "  {} steps · {} ms · seed {seed}",
            steps.len(),
            duration_ms(&steps)
        );
        println!("{rule}");
        println!("{}", draw(&steps, from, to, w, h, bounds));
        println!("{bot}");
    }

    // The property that matters, stated in numbers rather than as a drawing.
    let s = path(from, to, bounds, &Profile::default(), 1);
    let last = s.last().unwrap();
    println!(
        "\n  The last step is exactly the target: ({}, {}) == {to:?}\n",
        last.x, last.y
    );
}
