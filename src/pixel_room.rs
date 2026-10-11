//! A tiny hand-drawn room. Pixels are composed off-screen, then presented with
//! nearest-neighbour scaling; no runtime assets or per-pixel GDI objects.

pub const WIDTH: usize = 216;
pub const HEIGHT: usize = 48;
pub const IDLE_FRAMES: usize = 8;
pub const ACTION_FRAMES: usize = 48;

struct Canvas {
    pixels: Vec<u32>,
}

impl Canvas {
    fn rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32) {
        for row in y.max(0)..(y + h).min(HEIGHT as i32) {
            let start = row as usize * WIDTH + x.clamp(0, WIDTH as i32) as usize;
            let end = row as usize * WIDTH + (x + w).clamp(0, WIDTH as i32) as usize;
            if start < end {
                self.pixels[start..end].fill(color);
            }
        }
    }

    fn sprite(&mut self, x: i32, y: i32, rows: &[&str], palette: &[(u8, u32)], flip: bool) {
        for (dy, row) in rows.iter().enumerate() {
            for (dx, ch) in row.bytes().enumerate() {
                if let Some((_, color)) = palette.iter().find(|(key, _)| *key == ch) {
                    let dx = if flip { row.len() - 1 - dx } else { dx };
                    self.rect(x + dx as i32, y + dy as i32, 1, 1, *color);
                }
            }
        }
    }
}

struct Palette {
    wall: u32,
    wall_shade: u32,
    floor: u32,
    seam: u32,
    ink: u32,
    wood: u32,
    wood_light: u32,
    wood_shade: u32,
    sky: u32,
    horizon: u32,
    distant: u32,
    light: u32,
}

fn palette(hour: u16) -> Palette {
    let night = !(5..20).contains(&hour);
    Palette {
        wall: if night { 0x424354 } else { 0xf2e6d8 },
        wall_shade: if night { 0x393c4c } else { 0xe5d3c6 },
        floor: if night { 0x77646c } else { 0xc5a18e },
        seam: if night { 0x65565f } else { 0xb38d7c },
        ink: if night { 0x292d3d } else { 0x66555c },
        wood: if night { 0x9b7671 } else { 0xb98269 },
        wood_light: if night { 0xbc9282 } else { 0xe2b18a },
        wood_shade: if night { 0x6a525d } else { 0x926555 },
        sky: match hour {
            5..=8 => 0xd9aeb0,
            9..=16 => 0x9bc6ce,
            17..=19 => 0xcc8e99,
            _ => 0x303e60,
        },
        horizon: match hour {
            5..=8 => 0xf4d4b4,
            9..=16 => 0xd7e4d5,
            17..=19 => 0xf0bb98,
            _ => 0x656582,
        },
        distant: if night { 0x484e6e } else { 0x8da5a2 },
        light: if night { 0xf7d99e } else { 0xffe6b4 },
    }
}

fn plant(c: &mut Canvas, x: i32, y: i32, p: &Palette) {
    c.sprite(
        x,
        y,
        &[
            ".....d....",
            "..ll.d.ll.",
            ".dllldllld",
            "..dlldlld.",
            "....dd....",
            "...rrrr...",
            "...rRrr...",
            "....rr....",
        ],
        &[
            (b'd', 0x577b70),
            (b'l', 0x87a882),
            (b'r', p.wood),
            (b'R', p.wood_light),
        ],
        false,
    );
}

/// RGB pixels in the layout expected by a 32-bit BI_RGB DIB (0x00RRGGBB).
/// Frames 0..IDLE_FRAMES are quiet loops; subsequent ranges are cat stories.
pub fn render(hour: u16, frame: usize, background: u32) -> Vec<u32> {
    let p = palette(hour);
    let mut c = Canvas {
        pixels: vec![background; WIDTH * HEIGHT],
    };
    let night = !(5..20).contains(&hour);
    let beat = if frame < IDLE_FRAMES {
        frame
    } else {
        ((frame - IDLE_FRAMES) / 3) % IDLE_FRAMES
    };

    // Open plaster islands, rather than a rectangular room pasted onto the card.
    // Discrete colour steps preserve pixel edges while easing into either theme.
    let tint = |color: u32, weight: u32| -> u32 {
        let channel = |shift: u32| {
            ((((background >> shift) & 255) * (100 - weight) + ((color >> shift) & 255) * weight)
                / 100)
                << shift
        };
        channel(16) | channel(8) | channel(0)
    };
    c.rect(76, 9, 31, 28, tint(p.wall, 22));
    c.rect(79, 12, 29, 24, tint(p.wall, 38));
    c.rect(109, 1, 54, 31, tint(p.wall, 18));
    c.rect(107, 5, 58, 24, tint(p.wall, 18));
    c.rect(110, 3, 52, 27, tint(p.wall, 38));
    c.rect(113, 6, 47, 25, tint(p.wall, 58));

    // One continuous floor grounds the entire room. Only the outer silhouette
    // steps inward, leaving a clean edge against the card.
    c.rect(0, 35, 216, 13, p.floor);
    for y in [39, 44, 47] {
        c.rect(0, y, 216, 1, p.seam);
    }
    for (x, y) in [
        (18, 35),
        (59, 35),
        (108, 35),
        (157, 35),
        (200, 35),
        (37, 40),
        (84, 40),
        (136, 40),
        (182, 40),
        (15, 45),
        (67, 45),
        (117, 45),
        (163, 45),
    ] {
        c.rect(x, y, 1, 4, p.seam);
    }
    // Sparse grain, intentional rather than noisy texture.
    for (x, y, w) in [
        (3, 37, 9),
        (69, 37, 11),
        (112, 42, 10),
        (44, 46, 9),
        (190, 46, 12),
    ] {
        c.rect(x, y, w, 1, p.wood_light);
    }
    for y in 35..48 {
        let (left, right) = match y {
            35..=38 => (7, 213),
            39..=42 => (4, 214),
            43..=44 => (8, 212),
            45..=46 => (14, 208),
            _ => (20, 202),
        };
        for x in 0..WIDTH as i32 {
            if x < left || x >= right {
                c.rect(x, y, 1, 1, background);
            }
        }
    }
    // Hard-edged contact shadows, not a blurred vignette.
    c.rect(20, 47, 182, 1, tint(p.wood_shade, 18));

    // Recessed window: a miniature skyline, clouds and four glazed panes.
    c.rect(113, 3, 47, 28, p.wall_shade);
    c.rect(115, 3, 43, 26, p.ink);
    c.rect(116, 4, 41, 24, p.wood_light);
    c.rect(118, 5, 37, 21, p.sky);
    c.rect(118, 17, 37, 9, p.horizon);
    if night {
        c.sprite(
            143,
            7,
            &[".mmm.", "mm...", "mm...", ".mmm."],
            &[(b'm', p.light)],
            false,
        );
        for (x, y) in [(122, 7), (133, 9), (151, 14), (128, 13)] {
            c.rect(x, y, 1, 1, 0xd1d5e0);
        }
    } else {
        c.sprite(
            144,
            if hour >= 17 { 15 } else { 7 },
            &[".sss.", "sssss", "sssss", ".sss."],
            &[(b's', p.light)],
            false,
        );
        c.rect(122, 9, 7, 2, 0xf5eee0);
        c.rect(125, 8, 3, 1, 0xf5eee0);
        c.rect(135, 12, 7, 1, 0xf5eee0);
    }
    for (x, y, w) in [
        (118, 22, 7),
        (124, 20, 7),
        (131, 23, 8),
        (140, 21, 9),
        (149, 22, 6),
    ] {
        c.rect(x, y, w, 26 - y, p.distant);
        c.rect(x + 2, y + 2, 1, 1, if night { p.light } else { p.horizon });
    }
    c.rect(136, 5, 2, 21, p.wood_light);
    c.rect(118, 15, 37, 2, p.wood_light);
    c.rect(117, 26, 39, 2, p.wood);
    c.rect(113, 28, 47, 2, p.wood_light);
    c.rect(114, 30, 45, 1, p.wood_shade);
    // Rose linen curtains have fixed tops and gently moving lower hems.
    let sway = if matches!(beat, 3..=5) { 1 } else { 0 };
    for (x, sign) in [(111, 1), (154, -1)] {
        c.rect(x, 5, 7, 11, 0xb77980);
        c.rect(x + 1, 5, 2, 10, 0xd99d9a);
        c.rect(x + 4, 5, 1, 10, 0xe8b7a6);
        c.rect(x + 1, 15, 5, 2, p.wood_light);
        c.rect(x + sway * sign, 17, 7, 9, 0xc58a8c);
        c.rect(x + 1 + sway * sign, 17, 2, 9, 0xe1aba4);
        c.rect(x + 5 + sway * sign, 17, 1, 9, 0xa96e7b);
    }
    c.rect(109, 3, 54, 1, p.ink);
    c.rect(108, 3, 2, 2, p.wood_light);
    c.rect(162, 3, 2, 2, p.wood_light);
    if !night {
        // Stepped patches of sunlight across the floor.
        for (x, y, w) in [
            (115, 36, 15),
            (132, 36, 15),
            (111, 37, 15),
            (128, 37, 15),
            (107, 40, 15),
            (124, 40, 15),
            (103, 41, 15),
            (120, 41, 15),
        ] {
            c.rect(x, y, w, 1, p.light);
        }
    }

    // A floating shelf, wall print and clock give the room a lived-in scale.
    c.rect(7, 7, 20, 2, p.wood_light);
    c.rect(9, 9, 2, 2, p.wood_shade);
    c.rect(23, 9, 2, 2, p.wood_shade);
    c.rect(9, 3, 3, 4, 0x8da79e);
    c.rect(13, 2, 3, 5, 0xc38d83);
    c.rect(17, 3, 2, 4, 0xd9bd91);
    c.rect(21, 4, 4, 3, 0xa5b3b0);
    c.rect(21, 4, 4, 1, 0xe8d6b6);
    c.rect(58, 5, 15, 12, p.wood_shade);
    c.rect(59, 6, 13, 10, p.wood_light);
    c.rect(60, 7, 11, 8, if night { 0x68747e } else { 0xb9c9bc });
    c.rect(66, 8, 3, 3, p.light);
    c.sprite(
        60,
        11,
        &[".....gg....", "..gggggg...", ".ggggggggg.", "ggggggggggg"],
        &[(b'g', 0x658b7b)],
        false,
    );
    c.sprite(
        88,
        2,
        &[
            "..oooo..", ".oCCCCo.", "oCCdCCCo", "oCCdCCCo", "oCCdddCo", ".oCCCCo.", "..oooo..",
        ],
        &[(b'o', p.wood_shade), (b'C', 0xefdfc3), (b'd', p.ink)],
        false,
    );

    // A bookcase with inset shelves, book spines and a drawer below.
    c.rect(81, 12, 27, 27, p.wood_shade);
    c.rect(82, 12, 25, 2, p.wood_light);
    c.rect(83, 14, 23, 20, p.ink);
    for (x, h, color) in [
        (85, 7, 0x769692),
        (89, 8, 0xc08b84),
        (93, 6, 0xdec393),
        (97, 8, 0x8e91a4),
        (101, 7, 0xa9bda2),
    ] {
        c.rect(x, 22 - h, 3, h, color);
        c.rect(x, 20, 3, 1, p.light);
    }
    c.rect(83, 22, 23, 2, p.wood_light);
    c.rect(85, 29, 10, 3, 0xbf8c83);
    c.rect(85, 29, 10, 1, 0xe0b3a0);
    c.rect(87, 26, 9, 3, 0x7f9b99);
    c.rect(87, 26, 9, 1, 0xaec6b4);
    c.rect(99, 25, 5, 7, 0xdbc5a5);
    c.rect(100, 26, 3, 1, p.wood_shade);
    c.rect(83, 33, 23, 4, p.wood);
    c.rect(84, 33, 21, 1, p.wood_light);
    c.rect(92, 35, 5, 1, p.ink);
    c.rect(82, 38, 3, 3, p.wood_shade);
    c.rect(104, 38, 3, 3, p.wood_shade);
    plant(&mut c, 98, 5, &p);

    // Desk: bevelled top, tapered legs, screen stand, keyboard and coffee.
    c.rect(7, 39, 66, 2, p.seam);
    c.rect(9, 30, 3, 10, p.wood_shade);
    c.rect(10, 30, 1, 9, p.wood_light);
    c.rect(69, 30, 3, 10, p.wood_shade);
    c.rect(70, 30, 1, 9, p.wood_light);
    c.rect(8, 29, 64, 3, p.wood);
    c.rect(6, 27, 68, 2, p.wood_light);
    c.rect(6, 29, 68, 1, p.wood_shade);
    c.rect(9, 28, 63, 1, p.wood);
    c.rect(32, 8, 31, 18, p.ink);
    c.rect(33, 8, 29, 1, 0x9d9fa3);
    c.rect(34, 10, 27, 13, 0x293e49);
    c.rect(34, 10, 27, 2, 0x455763);
    for (x, color) in [(35, 0xd49b88), (37, 0xd8be86), (39, 0x8fb19b)] {
        c.rect(x, 10, 1, 1, color);
    }
    c.rect(35, 13, 3, 9, 0x354b57);
    for (x, y, w, color) in [
        (40, 13, 8, 0xa6c3a6),
        (42, 15, 11, 0xd4b6ad),
        (42, 17, 7, 0x91b6c4),
        (40, 19, 6, 0xa6c3a6),
    ] {
        c.rect(x, y, w, 1, color);
    }
    let progress = [2, 3, 5, 7, 8, 10, 11, 11][beat];
    c.rect(43, 21, progress, 1, 0xdcc895);
    if beat != 7 {
        c.rect(44 + progress, 21, 1, 1, p.light);
    }
    c.rect(45, 26, 7, 1, p.ink);
    c.rect(47, 24, 3, 2, p.ink);
    c.rect(43, 27, 17, 1, 0x877b82);
    c.rect(45, 26, 13, 1, 0xbbb3ab);
    c.rect(64, 24, 5, 3, 0xf2ddc0);
    c.rect(65, 24, 3, 1, 0x8b6b60);
    c.rect(69, 25, 1, 2, 0xf2ddc0);
    if matches!(beat, 1..=4) {
        c.rect(66, 21 - (beat / 3) as i32, 1, 2, p.wall_shade);
    }
    // Task lamp, always present, with a warm pool of light after dusk.
    c.rect(14, 25, 8, 2, p.ink);
    c.rect(17, 17, 2, 8, p.ink);
    c.rect(18, 15, 5, 2, p.ink);
    c.rect(22, 13, 2, 3, p.ink);
    c.sprite(
        19,
        12,
        &["..llll..", ".llllll.", "llllllll", ".gggggg."],
        &[
            (b'l', 0x718f89),
            (b'g', if night { p.light } else { 0xb5bdb0 }),
        ],
        false,
    );
    if night {
        c.rect(15, 27, 16, 1, p.light);
    }

    // Woven rug and wooden chair behind a clearly silhouetted seated coder.
    c.rect(21, 41, 37, 5, p.wood_shade);
    c.rect(23, 41, 33, 4, 0xa8787e);
    c.rect(25, 42, 29, 2, 0xc29291);
    for x in (22..58).step_by(3) {
        c.rect(x, 46, 1, 1, p.wood_light);
    }
    c.rect(28, 32, 20, 3, p.wood_shade);
    c.rect(29, 32, 18, 1, p.wood_light);
    c.rect(29, 35, 2, 9, p.ink);
    c.rect(45, 35, 2, 9, p.ink);
    c.sprite(
        28,
        14,
        &[
            ".......hhhhhh........",
            ".....hhhhHHhhhh......",
            "....hhhhHHHhhhhh.....",
            "....hhhHHHhhhhhh.....",
            "....hhhhhhhhhhhh.....",
            "....hhhhhhhhhHhh.....",
            "....hhhhhhhhhHhh.....",
            ".....hhhhhhhhhh......",
            "......hhsssshh.......",
            ".......ssssss........",
            ".....ttTTTTtttt......",
            "...tttTTTTTTtttt.....",
            "..tttTTTTTTTTtttt....",
            "..ttTTTTTTTTTtttt....",
            ".tttTTTTTTTTTTtttt...",
            ".tttTTTTTTTTTTtttt...",
            "..ttTTTTTTTTTTtttt...",
            "...tTTTTTTTTTTttt....",
            "...tttttttttttttt....",
            "...dddddddddddddd....",
            "....dddDDDDDDddd.....",
            "....dddDDDDDDddd.....",
            "....dddd..dddd.......",
            "....dddd..dddd.......",
            "....ddd....ddd.......",
            "...bbbb....bbbb......",
            "..bbBbb....bbBbb.....",
        ],
        &[
            (b'h', 0x493e50),
            (b'H', 0x6b5663),
            (b's', 0xd9a38a),
            (b't', 0x54777e),
            (b'T', 0x82aaa5),
            (b'd', 0x3e4c60),
            (b'D', 0x61738a),
            (b'b', p.ink),
            (b'B', 0x93868b),
        ],
        false,
    );
    let hand = if matches!(beat, 1 | 2 | 5) { 1 } else { 0 };
    c.rect(47, 28, 5, 2, 0x54777e);
    c.rect(51 + hand, 27, 3, 2, 0xe1b298);

    // Cat tower: sisal wrapping, cushioned ledges and a hanging toy.
    c.rect(167, 41, 21, 2, p.seam);
    c.rect(173, 18, 4, 24, p.wood_shade);
    c.rect(174, 18, 2, 23, 0xccaa8b);
    for y in (20..40).step_by(3) {
        c.rect(174, y, 2, 1, p.wood_shade);
    }
    c.rect(165, 17, 22, 3, p.wood_shade);
    c.rect(166, 16, 20, 2, 0xd4b4a0);
    c.rect(168, 15, 16, 1, 0xe5cdaf);
    c.rect(179, 29, 9, 3, p.wood_shade);
    c.rect(180, 28, 7, 2, 0xd4b4a0);
    c.rect(169, 40, 15, 3, p.wood);
    c.rect(169, 40, 15, 1, p.wood_light);
    c.rect(183, 20, 1, 5, p.wood_shade);
    c.rect(182 + sway, 25, 3, 3, 0xb77980);
    plant(&mut c, 199, 20, &p);
    c.rect(198, 28, 14, 2, p.wood_light);
    c.rect(200, 30, 2, 2, p.wood_shade);
    c.rect(209, 30, 2, 2, p.wood_shade);

    // Little ceramic water bowl and a knitted oval bed.
    c.rect(187, 41, 8, 2, p.seam);
    c.rect(187, 38, 7, 3, 0x6f949c);
    c.rect(188, 38, 5, 1, 0xbbd4cf);
    c.rect(188, 40, 5, 1, 0x8eb1b2);
    c.rect(195, 41, 19, 4, p.wood_shade);
    c.rect(193, 38, 21, 5, 0xa87375);
    c.rect(195, 36, 17, 7, 0xcb9890);
    c.rect(197, 36, 13, 6, 0xe9c8ac);
    c.rect(198, 37, 11, 3, 0xb68c84);

    let cat_palette = [
        (b'o', 0x574a57),
        (b'w', 0xf4e4ce),
        (b'c', 0xd6bca2),
        (b'p', 0x7b6670),
        (b'a', 0xc38b65),
        (b'n', 0xd99e95),
    ];
    if frame < IDLE_FRAMES {
        let breath = if matches!(beat, 2..=4) { -1 } else { 0 };
        c.sprite(
            195,
            31 + breath,
            &[
                "..o..o............",
                ".owooowo....ooo...",
                ".opwwwwo..oowwwo..",
                "owwwwwwwowwwawwwwo",
                "owppwwwwcwwaawwwwo",
                ".ownwwwcwwwwwwwwwo",
                "..ooowwwwwppwwwwo.",
                "....oooooooooooo..",
            ],
            &cat_palette,
            false,
        );
        if matches!(beat, 4..=6) {
            c.rect(204, 28 - (beat / 6) as i32, 2, 1, p.wood_shade);
            c.rect(205, 27 - (beat / 6) as i32, 1, 1, p.wood_shade);
        }
    } else {
        let action = (frame - IDLE_FRAMES) / ACTION_FRAMES;
        let step = (frame - IDLE_FRAMES) % ACTION_FRAMES;
        // Smooth out-and-back paths avoid teleporting home at the end.
        let (x, y, flip, paw) = match action {
            0 => {
                let x = if step < 18 {
                    195 - step as i32 * 8
                } else if step < 30 {
                    59
                } else {
                    59 + (step - 30) as i32 * 8
                };
                let y = match step {
                    16..=17 => 32 - (step - 15) as i32 * 3,
                    18..=29 => 23,
                    30..=31 => 23 + (step - 29) as i32 * 3,
                    _ => 32,
                };
                (x, y, step >= 30, (18..30).contains(&step))
            }
            1 => {
                let (x, y) = match step {
                    0..=7 => (195 - step as i32 * 3, 32),
                    8..=15 => (174 - (step - 8) as i32, 32 - (step - 8) as i32 * 4),
                    16..=29 => (167, 5),
                    30..=37 => (
                        167 + (step - 30) as i32,
                        (5 + (step - 30) as i32 * 4).min(32),
                    ),
                    38..=44 => (174 + (step - 38) as i32 * 3, 32),
                    _ => (195, 32),
                };
                (x, y, step >= 30, false)
            }
            _ => {
                let travel = if step < 24 { step } else { 47 - step };
                (195 - travel as i32 * 6, 32, step >= 24, false)
            }
        };
        c.sprite(
            x,
            y,
            &[
                "..o...o..........",
                ".owoooowo........",
                ".opwwwwo.........",
                "owwwwwwwwooooo...",
                "owpowwwwwwwawwo..",
                ".ownwwwcwwaawwwo.",
                "..oowwwcwwwppwwo.",
                "...owwwwwwwwwwo..",
                "....oooooooooo...",
            ],
            &cat_palette,
            flip,
        );
        let stride = if step % 4 < 2 { 0 } else { 1 };
        c.rect(x + 4 + stride, y + 9, 2, 2, 0xf4e4ce);
        c.rect(x + 11 - stride, y + 9, 2, 2, 0xf4e4ce);
        let tail_x = if flip { x - 2 } else { x + 15 };
        c.rect(tail_x, y + 3 - stride, 2, 4, 0x7b6670);
        c.rect(tail_x, y + 2 - stride, 1, 1, 0xf4e4ce);
        if paw {
            c.rect(x + 1, y + 5, 2, 4, 0xf4e4ce);
            c.rect(x, y + 4 - stride, 2, 2, 0xf4e4ce);
        }
    }
    // The bed lip occludes the resting cat, giving it a real cushion to lie in.
    c.rect(194, 41, 19, 2, 0xcb9890);
    c.rect(196, 41, 15, 1, 0xe9c8ac);
    for x in (196..211).step_by(3) {
        c.rect(x, 42, 1, 1, 0xa87375);
    }
    c.pixels
}
