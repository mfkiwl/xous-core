#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]

mod api;

mod backend;
use backend::XousDisplay;

mod op;

mod logo;
mod poweron;
mod sleep_note;

use api::{
    Circle, DrawStyle, Line, PixelColor, Point, Rectangle, RoundedRectangle, TextBounds, TextView,
};
use api::{ClipObject, ClipObjectType, Opcode};
use blitstr::GlyphStyle;
use blitstr_ref as blitstr;

mod blitstr2;
mod wordwrap;
#[macro_use]
mod style_macros;


use num_traits::FromPrimitive;
use xous::{msg_blocking_scalar_unpack, msg_scalar_unpack, MemoryRange};
use xous_ipc::Buffer;

mod fontmap;
use api::BulkRead;

fn draw_boot_logo(display: &mut XousDisplay) {
    display.blit_screen(&poweron::LOGO_MAP);
}

#[cfg(any(target_os = "none", target_os = "xous"))]
fn map_fonts() -> MemoryRange {
    log::trace!("mapping fonts");
    // this maps an extra page if the total length happens to fall on a 4096-byte boundary, but this is ok
    // because the reserved area is much larger
    let fontlen: u32 = ((fontmap::FONT_TOTAL_LEN as u32 + 8) & 0xFFFF_F000) + 0x1000;
    log::trace!(
        "requesting map of length 0x{:08x} at 0x{:08x}",
        fontlen,
        fontmap::FONT_BASE
    );
    let fontregion = xous::syscall::map_memory(
        xous::MemoryAddress::new(fontmap::FONT_BASE),
        None,
        fontlen as usize,
        xous::MemoryFlags::R,
    )
    .expect("couldn't map fonts");
    log::info!(
        "font base at virtual 0x{:08x}, len of 0x{:08x}",
        fontregion.as_ptr() as usize,
        usize::from(fontregion.len())
    );

    log::trace!(
        "mapping regular font to 0x{:08x}",
        fontregion.as_ptr() as usize + fontmap::REGULAR_OFFSET as usize
    );
    blitstr::map_font(blitstr::GlyphData::Emoji(
        (fontregion.as_ptr() as usize + fontmap::EMOJI_OFFSET) as usize,
    ));
    blitstr::map_font(blitstr::GlyphData::Hanzi(
        (fontregion.as_ptr() as usize + fontmap::HANZI_OFFSET) as usize,
    ));
    blitstr::map_font(blitstr::GlyphData::Regular(
        (fontregion.as_ptr() as usize + fontmap::REGULAR_OFFSET) as usize,
    ));
    blitstr::map_font(blitstr::GlyphData::Small(
        (fontregion.as_ptr() as usize + fontmap::SMALL_OFFSET) as usize,
    ));
    blitstr::map_font(blitstr::GlyphData::Bold(
        (fontregion.as_ptr() as usize + fontmap::BOLD_OFFSET) as usize,
    ));

    fontregion
}

#[cfg(not(any(target_os = "none", target_os = "xous")))]
fn map_fonts() -> MemoryRange {
    // does nothing
    let fontlen: u32 = ((fontmap::FONT_TOTAL_LEN as u32 + 8) & 0xFFFF_F000) + 0x1000;
    let fontregion = xous::syscall::map_memory(None, None, fontlen as usize, xous::MemoryFlags::R)
        .expect("couldn't map dummy memory for fonts");

    fontregion
}

#[xous::xous_main]
fn xmain() -> ! {
    log_server::init_wait().unwrap();
    let debugtv = false;
    log::set_max_level(log::LevelFilter::Info);
    log::info!("my PID is {}", xous::process::id());

    let xns = xous_names::XousNames::new().unwrap();
    // these connections should be established:
    // - GAM
    // - keyrom (for verifying font maps)
    let sid = xns
        .register_name(api::SERVER_NAME_GFX, Some(2))
        .expect("can't register server");

    // Create a new monochrome simulator display.
    let mut display = XousDisplay::new();
    //let mut fb = display.native_buffer();
    //let mut b2cursor = blitstr2::Cursor::new(0, 0, 16);
    //blitstr2::paint_str(&mut fb, blitstr2::ClipRect::new(0, 0, 100, 100), &mut b2cursor, "test");

    draw_boot_logo(&mut display);

    let fontregion = map_fonts();

    let mut use_sleep_note = true;
    if false {
        // leave this test case around
        // for some reason, the top right quadrant draws an extra pixel inside the fill area
        // when a fill color of "Light" is specified. However, if `None` fill is specified, it
        // works correctly. This is really puzzling, because the test for filled drawing happens
        // after the test for border drawing.
        use api::Point;
        let mut r = Rectangle::new(Point::new(20, 200), Point::new(151, 301));
        r.style = DrawStyle {
            fill_color: Some(PixelColor::Light),
            stroke_color: Some(PixelColor::Dark),
            stroke_width: 1,
        };
        let rr = RoundedRectangle::new(r, 16);
        op::rounded_rectangle(display.native_buffer(), rr, None);
    }

    let screen_clip = Rectangle::new(Point::new(0, 0), display.screen_size());

    display.redraw();

    // register a suspend/resume listener
    let sr_cid = xous::connect(sid).expect("couldn't create suspend callback connection");
    let mut susres = susres::Susres::new(None, &xns, Opcode::SuspendResume as u32, sr_cid)
        .expect("couldn't create suspend/resume object");

    let mut bulkread = BulkRead::default(); // holding buffer for bulk reads; wastes ~8k when not in use, but saves a lot of copy/init for each iteration of the read
    loop {
        let mut msg = xous::receive_message(sid).unwrap();
        log::trace!("Message: {:?}", msg);
        match FromPrimitive::from_usize(msg.body.id()) {
            Some(Opcode::SuspendResume) => xous::msg_scalar_unpack!(msg, token, _, _, _, {
                display.suspend(use_sleep_note);
                susres
                    .suspend_until_resume(token)
                    .expect("couldn't execute suspend/resume");
                display.resume(use_sleep_note);
            }),
            Some(Opcode::SetSleepNote) => xous::msg_scalar_unpack!(msg, set_use, _, _, _, {
                if set_use == 0 {
                    use_sleep_note = false;
                } else {
                    use_sleep_note = true;
                }
            }),
            Some(Opcode::DrawClipObject) => {
                let buffer =
                    unsafe { Buffer::from_memory_message(msg.body.memory_message().unwrap()) };
                let obj = buffer.to_original::<ClipObject, _>().unwrap();
                log::trace!("DrawClipObject {:?}", obj);
                match obj.obj {
                    ClipObjectType::Line(line) => {
                        op::line(display.native_buffer(), line, Some(obj.clip), false);
                    }
                    ClipObjectType::XorLine(line) => {
                        op::line(display.native_buffer(), line, Some(obj.clip), true);
                    }
                    ClipObjectType::Circ(circ) => {
                        op::circle(display.native_buffer(), circ, Some(obj.clip));
                    }
                    ClipObjectType::Rect(rect) => {
                        op::rectangle(display.native_buffer(), rect, Some(obj.clip));
                    }
                    ClipObjectType::RoundRect(rr) => {
                        op::rounded_rectangle(display.native_buffer(), rr, Some(obj.clip));
                    }
                }
            }
            Some(Opcode::DrawTextView) => {
                let mut buffer = unsafe {
                    Buffer::from_memory_message_mut(msg.body.memory_message_mut().unwrap())
                };
                let mut tv = buffer.to_original::<TextView, _>().unwrap();

                if tv.clip_rect.is_none() {
                    continue;
                } // if no clipping rectangle is specified, nothing to draw

                // this is the clipping rectangle of the canvas in screen coordinates
                let clip_rect = tv.clip_rect.unwrap();
                // the bounds hint is given in relative coordinates from the textview's origin. Translate it to screen coordinates.
                let bounds_hint_screen = tv.bounds_hint.translate(clip_rect.tl());

                let mut border = if let Some(mut precomputed) = tv.bounds_computed {
                    // preferentially use a pre-computed bounds, if there is one
                    // translate the precomputed rectangle to screen coordinates
                    precomputed.translate(clip_rect.tl());
                    blitstr2::ClipRect::new(
                        precomputed.tl().x as usize,
                        precomputed.tl().y as usize,
                        precomputed.br().x as usize,
                        precomputed.br().y as usize)
                } else {
                    match bounds_hint_screen {
                        // bounding box is literally this box, no other margining to be considered
                        TextBounds::BoundingBox(r) => {
                            blitstr2::ClipRect::new(
                                r.tl().x as usize,
                                r.tl().y as usize,
                                r.br().x as usize,
                                r.br().y as usize,
                            )
                        }
                        TextBounds::GrowableFromBr(br, width) => {
                            if !clip_rect.intersects_point(br) {
                                continue;
                            }
                            blitstr2::ClipRect::new(
                                if (br.x - width as i16) > (clip_rect.tl().x + tv.margin.x) {
                                    (br.x - width as i16) as usize
                                } else {
                                    (clip_rect.tl().x + tv.margin.x) as usize
                                },
                                (clip_rect.tl().y + tv.margin.y) as usize,
                                (br.x - tv.margin.x) as usize,
                                (br.y - tv.margin.y) as usize,
                            )
                        }
                        TextBounds::GrowableFromTl(tl, width) => {
                            if !clip_rect.intersects_point(tl) {
                                continue;
                            }
                            blitstr2::ClipRect::new(
                                (tl.x + tv.margin.x) as usize,
                                (tl.y + tv.margin.y) as usize,
                                if (tl.x + width as i16 + 2 * tv.margin.x) < clip_rect.br().x {
                                    (tl.x + width as i16 + tv.margin.x) as usize
                                } else {
                                    (clip_rect.br.x - tv.margin.x) as usize
                                },
                                (clip_rect.br.y - tv.margin.y) as usize
                            )
                        }
                        TextBounds::GrowableFromBl(bl, width) => {
                            blitstr2::ClipRect::new(
                                (bl.x + tv.margin.x) as usize,
                                (clip_rect.tl.y + tv.margin.y) as usize,
                                if (bl.x + width as i16 + 2 * tv.margin.x) < clip_rect.br().x {
                                    (bl.x + width as i16 + tv.margin.x) as usize
                                } else {
                                    (clip_rect.br().x - tv.margin.x) as usize
                                },
                                (bl.y - tv.margin.y) as usize
                            )
                        }
                    }
                };
                let mut overflow = false;
                log::info!("typesetting {} border {:?}, bounds {:?}", tv.text, border, bounds_hint_screen);
                let base_style = match tv.style { // a connector for now, we'll eventually depricate the old API
                    GlyphStyle::Small => blitstr2::GlyphStyle::Small,
                    GlyphStyle::Regular => blitstr2::GlyphStyle::Regular,
                    GlyphStyle::Bold => blitstr2::GlyphStyle::Bold,
                };
                let typeset_words = wordwrap::fit_str_to_clip(
                    tv.text.as_str().unwrap_or("UTF-8 error"),
                    &mut border,
                    &bounds_hint_screen,
                    /*&tv.style,*/ &base_style,
                    &mut tv.cursor,
                    &mut overflow);

                log::info!("wrapped to {:?}", border);

                // compute the clear rectangle -- the border is already in screen coordinates, just add the margin around it
                let mut clear_rect = border.to_rect();
                clear_rect.margin_out(tv.margin);

                let bordercolor = if tv.draw_border {
                    Some(PixelColor::Dark)
                } else {
                    None
                };
                let borderwidth: i16 = if tv.draw_border {
                    tv.border_width as i16
                } else {
                    0
                };
                let fillcolor = if tv.clear_area || tv.invert {
                    if tv.invert {
                        Some(PixelColor::Dark)
                    } else {
                        Some(PixelColor::Light)
                    }
                } else {
                    None
                };

                clear_rect.style = DrawStyle {
                    fill_color: fillcolor,
                    stroke_color: bordercolor,
                    stroke_width: borderwidth,
                };
                if !tv.dry_run() {
                    if tv.rounded_border.is_some() {
                        op::rounded_rectangle(
                            display.native_buffer(),
                            RoundedRectangle::new(clear_rect, tv.rounded_border.unwrap() as _),
                            Some(clear_rect),
                        );
                    } else {
                        op::rectangle(display.native_buffer(), clear_rect, tv.clip_rect);
                    }
                }

                if cfg!(feature = "braille") {
                    log::info!("{}", tv);
                }
                if !tv.dry_run() {
                    let mut insert_point = 0;
                    for word in typeset_words.iter() {
                        let mut p = word.origin.clone();
                        for glyph in word.gs.iter() {
                            /// TODO: need to redo the word wrap algorithm to not skip over whitespace entries so that insertion points work with multiple spaces
                            if let Some(ipoint) = tv.insertion {
                                if ipoint == insert_point {
                                    let top = Point::new(p.x as _, p.y as _);
                                    let bot = Point::new(p.x as _, (p.y + word.height) as _);
                                    let line = Line::new(top, bot);
                                    op::line(display.native_buffer(),
                                        line, Some(clear_rect), tv.invert);
                                }
                            }
                            // log::info!("drawing {:?} at {:?}", glyph, p);
                            blitstr2::xor_glyph(display.native_buffer(), &p, *glyph, tv.invert);
                            p.x += glyph.wide as usize; // words after word-wrapping are guaranteed to be on the same line
                            insert_point += 1;
                        }
                    }
                }

                log::info!("(TV): returning cursor of {:?}", tv.cursor);
                // pack our data back into the buffer to return
                buffer.replace(tv).unwrap();
            }
            Some(Opcode::Flush) => {
                display.update();
                display.redraw();
            }
            Some(Opcode::Clear) => {
                let mut r = Rectangle::full_screen();
                r.style = DrawStyle::new(PixelColor::Light, PixelColor::Light, 0);
                op::rectangle(display.native_buffer(), r, screen_clip.into())
            }
            Some(Opcode::Line) => msg_scalar_unpack!(msg, p1, p2, style, _, {
                let l =
                    Line::new_with_style(Point::from(p1), Point::from(p2), DrawStyle::from(style));
                op::line(display.native_buffer(), l, screen_clip.into(), false);
            }),
            Some(Opcode::Rectangle) => msg_scalar_unpack!(msg, tl, br, style, _, {
                let r = Rectangle::new_with_style(
                    Point::from(tl),
                    Point::from(br),
                    DrawStyle::from(style),
                );
                op::rectangle(display.native_buffer(), r, screen_clip.into());
            }),
            Some(Opcode::RoundedRectangle) => msg_scalar_unpack!(msg, tl, br, style, r, {
                let rr = RoundedRectangle::new(
                    Rectangle::new_with_style(
                        Point::from(tl),
                        Point::from(br),
                        DrawStyle::from(style),
                    ),
                    r as _,
                );
                op::rounded_rectangle(display.native_buffer(), rr, screen_clip.into());
            }),
            Some(Opcode::Circle) => msg_scalar_unpack!(msg, center, radius, style, _, {
                let c = Circle::new_with_style(
                    Point::from(center),
                    radius as _,
                    DrawStyle::from(style),
                );
                op::circle(display.native_buffer(), c, screen_clip.into());
            }),
            Some(Opcode::ScreenSize) => msg_blocking_scalar_unpack!(msg, _, _, _, _, {
                let pt = display.screen_size();
                xous::return_scalar2(msg.sender, pt.x as usize, pt.y as usize)
                    .expect("couldn't return ScreenSize request");
            }),
            Some(Opcode::QueryGlyphProps) => msg_blocking_scalar_unpack!(msg, style, _, _, _, {
                let glyph = GlyphStyle::from(style);
                xous::return_scalar2(
                    msg.sender,
                    glyph.into(),
                    blitstr::glyph_to_height_hint(glyph),
                )
                .expect("could not return QueryGlyphProps request");
            }),
            Some(Opcode::DrawSleepScreen) => msg_scalar_unpack!(msg, _, _, _, _, {
                display.blit_screen(&logo::LOGO_MAP);
                display.update();
                display.redraw();
            }),
            Some(Opcode::Devboot) => msg_scalar_unpack!(msg, ena, _, _, _, {
                if ena != 0 {
                    display.set_devboot(true);
                } else {
                    display.set_devboot(false);
                }
            }),
            Some(Opcode::RestartBulkRead) => msg_blocking_scalar_unpack!(msg, _, _, _, _, {
                bulkread.from_offset = 0;
                xous::return_scalar(msg.sender, 0)
                    .expect("couldn't ack that bulk read pointer was reset");
            }),
            Some(Opcode::BulkReadFonts) => {
                let fontlen = fontmap::FONT_TOTAL_LEN as u32 + 8;
                let mut buf = unsafe {
                    Buffer::from_memory_message_mut(msg.body.memory_message_mut().unwrap())
                };
                //let mut bulkread = buf.as_flat::<BulkRead, _>().unwrap(); // try to skip the copy/init step by using a persistent structure
                let fontslice = fontregion.as_slice::<u8>();
                assert!(fontlen <= fontslice.len() as u32);
                if bulkread.from_offset >= fontlen {
                    log::error!(
                        "BulkReadFonts attempt to read out of bound on the font area; ignoring!"
                    );
                    continue;
                }
                let readlen = if bulkread.from_offset + bulkread.buf.len() as u32 > fontlen {
                    // returns what is readable of the last bit; anything longer than the fontlen is undefined/invalid
                    fontlen as usize - bulkread.from_offset as usize
                } else {
                    bulkread.buf.len()
                };
                for (&src, dst) in fontslice
                    [bulkread.from_offset as usize..bulkread.from_offset as usize + readlen]
                    .iter()
                    .zip(bulkread.buf.iter_mut())
                {
                    *dst = src;
                }
                bulkread.len = readlen as u32;
                bulkread.from_offset += readlen as u32;
                buf.replace(bulkread).unwrap();
            }
            Some(Opcode::TestPattern) => msg_blocking_scalar_unpack!(msg, duration, _, _, _, {
                let mut stashmem = xous::syscall::map_memory(
                    None,
                    None,
                    ((backend::FB_SIZE * 4) + 4096) & !4095,
                    xous::MemoryFlags::R | xous::MemoryFlags::W,
                ).expect("couldn't map stash frame buffer");
                let stash = &mut stashmem.as_slice_mut()[..backend::FB_SIZE];
                for (&src, dst) in display.as_slice().iter().zip(stash.iter_mut()) {
                    *dst = src;
                }
                for lines in 0..backend::FB_LINES { // mark all lines dirty
                    stash[lines * backend::FB_WIDTH_WORDS + (backend::FB_WIDTH_WORDS - 1)] |= 0x1_0000;
                }

                let ticktimer = ticktimer_server::Ticktimer::new().unwrap();
                let start_time = ticktimer.elapsed_ms();
                let mut testmem = xous::syscall::map_memory(
                    None,
                    None,
                    ((backend::FB_SIZE * 4) + 4096) & !4095,
                    xous::MemoryFlags::R | xous::MemoryFlags::W,
                ).expect("couldn't map stash frame buffer");
                let testpat = &mut testmem.as_slice_mut()[..backend::FB_SIZE];
                const DWELL: usize = 1000;
                while ticktimer.elapsed_ms() - start_time < duration as u64 {
                    // all black
                    for w in testpat.iter_mut() {
                        *w = 0;
                    }
                    for lines in 0..backend::FB_LINES { // mark dirty bits
                        testpat[lines * backend::FB_WIDTH_WORDS + (backend::FB_WIDTH_WORDS - 1)] |= 0x1_0000;
                    }
                    display.blit_screen(testpat);
                    display.update();
                    display.redraw();
                    ticktimer.sleep_ms(DWELL).unwrap();

                    // all white
                    for w in testpat.iter_mut() {
                        *w = 0xFFFF_FFFF;
                    }
                    // dirty bits already set
                    display.blit_screen(testpat);
                    display.update();
                    display.redraw();
                    ticktimer.sleep_ms(DWELL).unwrap();

                    // vertical bars
                    for lines in 0..backend::FB_LINES {
                        for words in 0..backend::FB_WIDTH_WORDS {
                            testpat[lines * backend::FB_WIDTH_WORDS + words] = 0xaaaa_aaaa;
                        }
                    }
                    display.blit_screen(testpat);
                    display.update();
                    display.redraw();
                    ticktimer.sleep_ms(DWELL).unwrap();

                    for lines in 0..backend::FB_LINES {
                        for words in 0..backend::FB_WIDTH_WORDS {
                            testpat[lines * backend::FB_WIDTH_WORDS + words] = 0x5555_5555;
                        }
                    }
                    display.blit_screen(testpat);
                    display.update();
                    display.redraw();
                    ticktimer.sleep_ms(DWELL).unwrap();

                    // horiz bars
                    for lines in 0..backend::FB_LINES {
                        for words in 0..backend::FB_WIDTH_WORDS {
                            if lines % 2 == 0 {
                                testpat[lines * backend::FB_WIDTH_WORDS + words] = 0x0;
                            } else {
                                testpat[lines * backend::FB_WIDTH_WORDS + words] = 0xffff_ffff;
                            }
                        }
                        testpat[lines * backend::FB_WIDTH_WORDS + (backend::FB_WIDTH_WORDS - 1)] |= 0x1_0000;
                    }
                    display.blit_screen(testpat);
                    display.update();
                    display.redraw();
                    ticktimer.sleep_ms(DWELL).unwrap();

                    for lines in 0..backend::FB_LINES {
                        for words in 0..backend::FB_WIDTH_WORDS {
                            if lines % 2 == 1 {
                                testpat[lines * backend::FB_WIDTH_WORDS + words] = 0x0;
                            } else {
                                testpat[lines * backend::FB_WIDTH_WORDS + words] = 0xffff_ffff;
                            }
                        }
                        testpat[lines * backend::FB_WIDTH_WORDS + (backend::FB_WIDTH_WORDS - 1)] |= 0x1_0000;
                    }
                    display.blit_screen(testpat);
                    display.update();
                    display.redraw();
                    ticktimer.sleep_ms(DWELL).unwrap();

                }
                display.blit_screen(stash);

                xous::return_scalar(msg.sender, duration).expect("couldn't ack test pattern");
            }),
            Some(Opcode::Quit) => break,
            None => {
                log::error!("received opcode scalar that is not handled");
            }
        }
    }
    log::trace!("main loop exit, destroying servers");
    xns.unregister_server(sid).unwrap();
    xous::destroy_server(sid).unwrap();
    log::trace!("quitting");
    xous::terminate_process(0)
}
