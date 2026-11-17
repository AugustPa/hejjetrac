use hejjetrac_core::{Config, Diagnostics, Geometry, GrayFrame, Sphere};

#[test]
fn cropped_sphere_keeps_off_sensor_roi_and_atlas_cells_unknown() -> hejjetrac_core::Result<()> {
    use glam::{DQuat, DVec3};
    use std::f64::consts::{PI, TAU};

    for fisheye in [false, true] {
        // The first geometry reproduces the supplied 320x240 recording's
        // cropped top/bottom. Off-axis controls also cross the side edges.
        for centre in [
            [
                0.021766231744497447,
                0.013513283470233292,
                0.9996717572911109,
            ],
            DVec3::new(0.20, 0.10, 1.0).normalize().to_array(),
            DVec3::new(-0.20, -0.10, 1.0).normalize().to_array(),
        ] {
            let config = Config {
                width: 320,
                height: 240,
                vfov_deg: 22.0,
                fisheye,
                sphere: Sphere {
                    centre,
                    angular_radius: 0.23955510537951344,
                },
                ..Config::default()
            };
            let geometry = Geometry::new(config.clone())?;
            let frame = GrayFrame::new(320, 240, vec![0.5; 320 * 240], 0.0)?;
            let mut diagnostics = Diagnostics::new(config.clone(), 6, 1.25, 0.25)?;
            let views = diagnostics.update(&frame, None);
            let n = views.roi_size;
            let radius = config.sphere.angular_radius;
            let centre = DVec3::from_array(centre);
            let basis = DQuat::from_rotation_arc(centre, DVec3::Z);
            let mut observed_cells = vec![false; views.accumulated.len()];
            let mut cropped = 0;
            let mut visible = 0;
            for y in 0..n {
                for x in 0..n {
                    let dx = (x as f64 + 0.5 - n as f64 * 0.5) * 2.0 * radius / n as f64;
                    let dy = (y as f64 + 0.5 - n as f64 * 0.5) * 2.0 * radius / n as f64;
                    let theta = dx.hypot(dy);
                    if theta >= radius * 0.975 {
                        continue;
                    }
                    let factor = if theta > 1e-10 {
                        theta.sin() / theta
                    } else {
                        1.0
                    };
                    let ray = basis.inverse() * DVec3::new(dx * factor, dy * factor, theta.cos());
                    let xy = geometry.project_ray(ray).ok_or_else(|| {
                        hejjetrac_core::Error::Config("test ray cannot project".into())
                    })?;
                    if frame.sample(xy[0] - 0.5, xy[1] - 0.5).is_none() {
                        cropped += 1;
                        assert_eq!(
                            views.roi[y * n + x],
                            128,
                            "off-sensor sample became a binary observation: {xy:?}"
                        );
                        continue;
                    }
                    visible += 1;
                    assert_ne!(views.roi[y * n + x], 128);
                    let dot = ray.dot(centre);
                    let r = radius.sin();
                    let point =
                        basis * ((ray * (dot - (dot * dot + r * r - 1.0).sqrt()) - centre) / r);
                    let mx = (((-point.x.atan2(point.z) - PI) / TAU).rem_euclid(1.0)
                        * views.map_width as f64)
                        .floor() as usize;
                    let my = ((point.y + 1.0) * 0.5 * views.map_height as f64).floor() as usize;
                    observed_cells[my.min(views.map_height - 1) * views.map_width
                        + mx.min(views.map_width - 1)] = true;
                }
            }
            assert!(
                cropped > 100 && visible > 1000,
                "fixture must retain a cropped visible ball"
            );
            for (index, value) in views.accumulated.iter().enumerate() {
                assert_eq!(
                    *value != 128,
                    observed_cells[index],
                    "atlas cell {index} must have an actual source observation"
                );
            }
        }
    }
    Ok(())
}

#[test]
fn wide_angle_off_axis_sphere_only_intersects_forward_ray_half_line() -> hejjetrac_core::Result<()>
{
    let config = Config {
        width: 320,
        height: 160,
        vfov_deg: 169.,
        sphere: Sphere {
            centre: glam::DVec3::new(-1., 0., 0.05).normalize().to_array(),
            angular_radius: 0.10,
        },
        ..Config::default()
    };
    let geometry = Geometry::new(config.clone())?;
    let visible = geometry
        .surface(1., 80.)
        .ok_or_else(|| hejjetrac_core::Error::Config("visible test sphere is missing".into()))?;
    let point = glam::DVec3::from_array(config.sphere.centre)
        + visible * config.sphere.angular_radius.sin();
    assert!(point.z > 0.);
    let projected = geometry.project(visible).ok_or_else(|| {
        hejjetrac_core::Error::Config("visible test surface cannot project".into())
    })?;
    assert!((projected[0] - 1.).abs() < 1e-9);
    assert!((projected[1] - 80.).abs() < 1e-9);
    // The opposite image edge intersects the same infinite line behind the
    // camera, but that is not a surface visible along this pixel's forward ray.
    assert!(geometry.surface(319., 80.).is_none());
    Ok(())
}

#[test]
fn small_odd_source_uses_legacy_roi_and_even_atlas_dimensions() -> hejjetrac_core::Result<()> {
    let config = Config {
        width: 41,
        height: 64,
        ..Config::default()
    };
    let mut diagnostics = Diagnostics::new(config, 6, 1.25, 0.25)?;
    let views = diagnostics.update(&GrayFrame::new(41, 64, vec![0.5; 41 * 64], 0.0)?, None);
    assert_eq!(views.roi_size, 41);
    assert_eq!((views.map_width, views.map_height), (122, 61));
    assert_eq!(views.accumulated.len(), 122 * 61);
    assert_eq!(views.instant.len(), 122 * 61);
    Ok(())
}
