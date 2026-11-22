// Feature test for the Kinect SDK v1.8 managed API (Microsoft.Kinect.dll) on the
// WinUSB runtime: every color / depth format, near mode, emitter off, coordinate
// mapping, camera settings, tilt + accelerometer, audio, restart and hotplug.
//
// Build (C# 5, .NET Framework csc), then put the proxy kinect10.dll next to the exe:
//   csc -platform:x64 -out:KinectFeatures.exe -r:<GAC>\Microsoft.Kinect.dll KinectFeatures.cs
// Run:  KinectFeatures [all|color|depth|mapper|emitter|camera|tilt|audio|restart] [--out dir]
//       KinectFeatures hotplug <seconds>   (unplug and replug the sensor while it waits)
using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Linq;
using System.Threading;
using Microsoft.Kinect;

static class KinectFeatures
{
    static int failures;
    static string outDir = ".";

    static void Report(bool ok, string name, string detail)
    {
        if (!ok) failures++;
        Console.WriteLine("[{0}] {1}: {2}", ok ? "PASS" : "FAIL", name, detail);
    }

    static void Info(string name, string detail)
    {
        Console.WriteLine("[INFO] {0}: {1}", name, detail);
    }

    static int Main(string[] args)
    {
        var tests = new List<string>();
        int hotplugSeconds = 0;
        for (int i = 0; i < args.Length; i++)
        {
            if (args[i] == "--out") outDir = args[++i];
            else if (args[i] == "hotplug") hotplugSeconds = int.Parse(args[++i]);
            else tests.Add(args[i]);
        }
        Directory.CreateDirectory(outDir);
        if (tests.Count == 0 || tests.Contains("all"))
            tests = new List<string> { "color", "depth", "mapper", "emitter", "camera", "tilt", "audio", "restart" };
        Console.WriteLine("process: {0}-bit", IntPtr.Size * 8);

        if (hotplugSeconds > 0) return Hotplug(hotplugSeconds);

        var sensor = KinectSensor.KinectSensors.FirstOrDefault(k => k.Status == KinectStatus.Connected);
        if (sensor == null) { Console.WriteLine("no connected sensor"); return 1; }
        Info("sensor", string.Format("id {0}, connection {1}", sensor.UniqueKinectId, sensor.DeviceConnectionId));

        sensor.DepthStream.Enable(DepthImageFormat.Resolution320x240Fps30);
        sensor.ColorStream.Enable(ColorImageFormat.RgbResolution640x480Fps30);
        sensor.Start();
        foreach (var t in tests)
        {
            try
            {
                switch (t)
                {
                    case "color": TestColor(sensor); break;
                    case "depth": TestDepth(sensor); break;
                    case "mapper": TestMapper(sensor); break;
                    case "emitter": TestEmitter(sensor); break;
                    case "camera": TestCameraSettings(sensor); break;
                    case "tilt": TestTilt(sensor); break;
                    case "audio": TestAudio(sensor); break;
                    case "restart": TestRestart(sensor); break;
                    default: Console.WriteLine("unknown test {0}", t); break;
                }
            }
            catch (Exception e)
            {
                string where = (e.StackTrace ?? "").Split('\n')[0].Trim();
                Report(false, t, "exception " + e.GetType().Name + ": " + e.Message + " @ " + where);
            }
        }
        sensor.Stop();
        Console.WriteLine("{0} failure(s)", failures);
        return failures;
    }

    // ---------------------------------------------------------------- color

    struct Capture
    {
        public int Frames, Width, Height, Bpp, Timeouts;
        public double Fps;
        public byte[] Last, Prev;
    }

    static Capture CaptureColor(KinectSensor s, double seconds)
    {
        var c = new Capture();
        var sw = Stopwatch.StartNew();
        double first = -1, last = 0;
        while (sw.Elapsed.TotalSeconds < seconds)
        {
            using (var f = s.ColorStream.OpenNextFrame(500))
            {
                if (f == null) { c.Timeouts++; continue; }
                if (c.Last == null || c.Last.Length != f.PixelDataLength)
                {
                    c.Last = new byte[f.PixelDataLength];
                    c.Prev = null;
                }
                else
                {
                    if (c.Prev == null) c.Prev = new byte[c.Last.Length];
                    var tmp = c.Prev; c.Prev = c.Last; c.Last = tmp;
                }
                f.CopyPixelDataTo(c.Last);
                c.Frames++;
                c.Width = f.Width; c.Height = f.Height; c.Bpp = f.BytesPerPixel;
                last = sw.Elapsed.TotalSeconds;
                if (first < 0) first = last;
            }
        }
        c.Fps = c.Frames > 1 ? (c.Frames - 1) / (last - first) : 0;
        return c;
    }

    // Luminance (0..255) of every pixel of a color/IR frame.
    static byte[] Luma(ColorImageFormat fmt, byte[] d, int w, int h)
    {
        var y = new byte[w * h];
        for (int i = 0; i < w * h; i++)
        {
            switch (fmt)
            {
                case ColorImageFormat.InfraredResolution640x480Fps30:
                    y[i] = d[2 * i + 1]; // 16-bit little endian, 10 significant bits at the top
                    break;
                case ColorImageFormat.RawBayerResolution640x480Fps30:
                case ColorImageFormat.RawBayerResolution1280x960Fps12:
                    y[i] = d[i];
                    break;
                case ColorImageFormat.RawYuvResolution640x480Fps15:
                    y[i] = d[2 * i + 1]; // UYVY: Y is every odd byte
                    break;
                default: // BGRX
                    y[i] = (byte)((d[4 * i] * 29 + d[4 * i + 1] * 150 + d[4 * i + 2] * 77) >> 8);
                    break;
            }
        }
        return y;
    }

    static void Stats(byte[] y, out double mean, out double sd)
    {
        double s = 0, s2 = 0;
        foreach (var v in y) { s += v; s2 += v * v; }
        mean = s / y.Length;
        sd = Math.Sqrt(Math.Max(0, s2 / y.Length - mean * mean));
    }

    static void SavePgm(string name, byte[] y, int w, int h)
    {
        var header = System.Text.Encoding.ASCII.GetBytes(string.Format("P5\n{0} {1}\n255\n", w, h));
        using (var fs = File.Create(Path.Combine(outDir, name)))
        {
            fs.Write(header, 0, header.Length);
            fs.Write(y, 0, w * h);
        }
    }

    static void TestColor(KinectSensor s)
    {
        var formats = new[]
        {
            new { F = ColorImageFormat.RgbResolution640x480Fps30, W = 640, H = 480, Bpp = 4, Fps = 30 },
            new { F = ColorImageFormat.RgbResolution1280x960Fps12, W = 1280, H = 960, Bpp = 4, Fps = 12 },
            new { F = ColorImageFormat.YuvResolution640x480Fps15, W = 640, H = 480, Bpp = 4, Fps = 15 },
            new { F = ColorImageFormat.RawYuvResolution640x480Fps15, W = 640, H = 480, Bpp = 2, Fps = 15 },
            new { F = ColorImageFormat.RawBayerResolution640x480Fps30, W = 640, H = 480, Bpp = 1, Fps = 30 },
            new { F = ColorImageFormat.RawBayerResolution1280x960Fps12, W = 1280, H = 960, Bpp = 1, Fps = 12 },
            new { F = ColorImageFormat.InfraredResolution640x480Fps30, W = 640, H = 480, Bpp = 2, Fps = 30 },
            new { F = ColorImageFormat.RgbResolution640x480Fps30, W = 640, H = 480, Bpp = 4, Fps = 30 },
        };
        foreach (var x in formats)
        {
            s.ColorStream.Enable(x.F);
            CaptureColor(s, 1.0); // let the mode switch settle
            var c = CaptureColor(s, 4.0);
            string name = "color " + x.F;
            if (c.Frames == 0) { Report(false, name, "no frames"); continue; }
            var y = Luma(x.F, c.Last, c.Width, c.Height);
            double mean, sd, diff = 0;
            Stats(y, out mean, out sd);
            if (c.Prev != null)
            {
                var p = Luma(x.F, c.Prev, c.Width, c.Height);
                for (int i = 0; i < y.Length; i++) diff += Math.Abs(y[i] - p[i]);
                diff /= y.Length;
            }
            SavePgm(x.F + ".pgm", y, c.Width, c.Height);
            bool ok = c.Width == x.W && c.Height == x.H && c.Bpp == x.Bpp
                && c.Fps > x.Fps * 0.85 && c.Fps < x.Fps * 1.15 && sd > 2 && diff > 0;
            Report(ok, name, string.Format(
                "{0}x{1}x{2}, {3} frames {4:F1} fps, timeouts {5}, luma mean {6:F0} sd {7:F0}, frame-to-frame diff {8:F2}",
                c.Width, c.Height, c.Bpp, c.Frames, c.Fps, c.Timeouts, mean, sd, diff));
        }
    }

    // ---------------------------------------------------------------- depth

    struct DepthCapture
    {
        public int Frames, Width, Height, Timeouts;
        public double Fps;
        public DepthImagePixel[] Last;
    }

    static DepthCapture CaptureDepth(KinectSensor s, double seconds)
    {
        var c = new DepthCapture();
        var sw = Stopwatch.StartNew();
        double first = -1, last = 0;
        while (sw.Elapsed.TotalSeconds < seconds)
        {
            using (var f = s.DepthStream.OpenNextFrame(500))
            {
                if (f == null) { c.Timeouts++; continue; }
                if (c.Last == null || c.Last.Length != f.PixelDataLength)
                    c.Last = new DepthImagePixel[f.PixelDataLength];
                f.CopyDepthImagePixelDataTo(c.Last);
                c.Frames++;
                c.Width = f.Width; c.Height = f.Height;
                last = sw.Elapsed.TotalSeconds;
                if (first < 0) first = last;
            }
        }
        c.Fps = c.Frames > 1 ? (c.Frames - 1) / (last - first) : 0;
        return c;
    }

    static string DepthSummary(DepthImagePixel[] px, out double knownFraction)
    {
        var known = px.Where(p => p.IsKnownDepth).Select(p => (int)p.Depth).OrderBy(d => d).ToArray();
        knownFraction = known.Length / (double)px.Length;
        int players = px.Count(p => p.PlayerIndex != 0);
        if (known.Length == 0) return "no known depth";
        return string.Format("known {0:P0}, depth min {1} median {2} max {3} mm, player pixels {4}",
            knownFraction, known[0], known[known.Length / 2], known[known.Length - 1], players);
    }

    static void SaveDepth(string name, DepthImagePixel[] px, int w, int h)
    {
        var y = new byte[w * h];
        for (int i = 0; i < y.Length; i++)
            y[i] = px[i].IsKnownDepth ? (byte)Math.Max(1, 255 - Math.Min(4000, (int)px[i].Depth) * 255 / 4000) : (byte)0;
        SavePgm(name, y, w, h);
    }

    static void TestDepth(KinectSensor s)
    {
        foreach (var fmt in new[] { DepthImageFormat.Resolution640x480Fps30, DepthImageFormat.Resolution320x240Fps30, DepthImageFormat.Resolution80x60Fps30 })
        {
            s.DepthStream.Enable(fmt);
            CaptureDepth(s, 1.0);
            var c = CaptureDepth(s, 4.0);
            string name = "depth " + fmt;
            if (c.Frames == 0) { Report(false, name, "no frames"); continue; }
            double known;
            string summary = DepthSummary(c.Last, out known);
            SaveDepth(fmt + ".pgm", c.Last, c.Width, c.Height);
            Report(c.Fps > 25 && known > 0.2, name, string.Format("{0}x{1}, {2} frames {3:F1} fps, {4}",
                c.Width, c.Height, c.Frames, c.Fps, summary));
        }

        // Skeleton tracking modes need the depth stream at 320x240 or 640x480.
        s.DepthStream.Enable(DepthImageFormat.Resolution320x240Fps30);
        foreach (var mode in new[] { SkeletonTrackingMode.Default, SkeletonTrackingMode.Seated })
        {
            s.SkeletonStream.TrackingMode = mode;
            s.SkeletonStream.Enable();
            var skel = new Skeleton[6];
            int frames = 0, tracked = 0;
            var sw = Stopwatch.StartNew();
            while (sw.Elapsed.TotalSeconds < 4)
            {
                s.DepthStream.OpenNextFrame(0);
                using (var f = s.SkeletonStream.OpenNextFrame(200))
                {
                    if (f == null) continue;
                    frames++;
                    f.CopySkeletonDataTo(skel);
                    if (skel.Any(k => k.TrackingState == SkeletonTrackingState.Tracked)) tracked++;
                }
            }
            s.SkeletonStream.Disable();
            Report(frames > 90, "skeleton " + mode, string.Format("{0} frames in 4 s, {1} with a tracked person", frames, tracked));
        }

        // Near range is a Kinect for Windows feature; the SDK refuses it on Xbox 360 sensors.
        try
        {
            s.DepthStream.Range = DepthRange.Near;
            CaptureDepth(s, 1.0);
            var c = CaptureDepth(s, 3.0);
            double known;
            Report(c.Fps > 25, "depth near range", string.Format("{0:F1} fps, {1}", c.Fps, DepthSummary(c.Last, out known)));
            s.DepthStream.Range = DepthRange.Default;
        }
        catch (InvalidOperationException e)
        {
            Info("depth near range", "refused by the SDK: " + e.Message);
        }
    }

    // ---------------------------------------------------------------- mapper

    static void TestMapper(KinectSensor s)
    {
        s.ColorStream.Enable(ColorImageFormat.RgbResolution640x480Fps30);
        s.DepthStream.Enable(DepthImageFormat.Resolution640x480Fps30);
        CaptureDepth(s, 1.0);
        var c = CaptureDepth(s, 0.5);
        var pts = new ColorImagePoint[c.Last.Length];
        s.CoordinateMapper.MapDepthFrameToColorFrame(DepthImageFormat.Resolution640x480Fps30, c.Last,
            ColorImageFormat.RgbResolution640x480Fps30, pts);
        double dx = 0, dy = 0; int n = 0, inside = 0;
        for (int i = 0; i < pts.Length; i++)
        {
            if (!c.Last[i].IsKnownDepth) continue;
            n++;
            dx += pts[i].X - i % 640;
            dy += pts[i].Y - i / 640;
            if (pts[i].X >= 0 && pts[i].X < 640 && pts[i].Y >= 0 && pts[i].Y < 480) inside++;
        }
        if (n == 0) { Report(false, "coordinate mapper", "no known depth"); return; }
        dx /= n; dy /= n;
        // The color camera sits ~2.5 cm beside the IR camera, so a real calibration
        // gives a shift of a few pixels to a few tens of pixels, never exactly zero.
        bool ok = (Math.Abs(dx) > 0.5 || Math.Abs(dy) > 0.5) && Math.Abs(dx) < 80 && Math.Abs(dy) < 80;
        var center = new DepthImagePoint { X = 320, Y = 240, Depth = 2000 };
        var sp = s.CoordinateMapper.MapDepthPointToSkeletonPoint(DepthImageFormat.Resolution640x480Fps30, center);
        Report(ok, "coordinate mapper", string.Format(
            "depth->color mean shift ({0:F1}, {1:F1}) px, {2:P0} inside the color frame; depth (320,240,2000mm) -> skeleton ({3:F3}, {4:F3}, {5:F3}) m",
            dx, dy, inside / (double)n, sp.X, sp.Y, sp.Z));
        s.DepthStream.Enable(DepthImageFormat.Resolution320x240Fps30);
    }

    // ---------------------------------------------------------------- emitter

    static void TestEmitter(KinectSensor s)
    {
        double on1, off, on2;
        CaptureDepth(s, 0.5);
        DepthSummary(CaptureDepth(s, 0.5).Last, out on1);
        try
        {
            s.ForceInfraredEmitterOff = true;
        }
        catch (InvalidOperationException e)
        {
            Info("emitter off", "refused by the SDK: " + e.Message);
            return;
        }
        Thread.Sleep(1000);
        DepthSummary(CaptureDepth(s, 0.5).Last, out off);
        s.ForceInfraredEmitterOff = false;
        Thread.Sleep(1500);
        DepthSummary(CaptureDepth(s, 0.5).Last, out on2);
        Report(on1 > 0.2 && off < on1 * 0.2 && on2 > on1 * 0.7, "emitter off/on",
            string.Format("known depth {0:P0} -> off {1:P0} -> on {2:P0}", on1, off, on2));
    }

    // ---------------------------------------------------------------- camera settings

    static double MeanLuma(KinectSensor s)
    {
        CaptureColor(s, 1.0);
        var c = CaptureColor(s, 0.5);
        double mean, sd;
        Stats(Luma(ColorImageFormat.RgbResolution640x480Fps30, c.Last, c.Width, c.Height), out mean, out sd);
        return mean;
    }

    static void TestCameraSettings(KinectSensor s)
    {
        s.ColorStream.Enable(ColorImageFormat.RgbResolution640x480Fps30);
        ColorCameraSettings cs;
        try { cs = s.ColorStream.CameraSettings; }
        catch (InvalidOperationException e)
        {
            // Color camera settings are a Kinect for Windows feature.
            Info("camera settings", "refused by the SDK: " + e.Message);
            return;
        }
        Info("camera settings", string.Format(
            "auto exposure {0}, exposure {1} [{2}..{3}], gain {4}, brightness {5}, contrast {6}, saturation {7}, auto WB {8}, WB {9}",
            cs.AutoExposure, cs.ExposureTime, cs.MinExposureTime, cs.MaxExposureTime, cs.Gain,
            cs.Brightness, cs.Contrast, cs.Saturation, cs.AutoWhiteBalance, cs.WhiteBalance));
        double auto = MeanLuma(s);
        cs.AutoExposure = false;
        cs.Gain = cs.MinGain;
        cs.ExposureTime = cs.MinExposureTime + (cs.MaxExposureTime - cs.MinExposureTime) * 0.005;
        double dark = MeanLuma(s);
        cs.ExposureTime = cs.MinExposureTime + (cs.MaxExposureTime - cs.MinExposureTime) * 0.1;
        double bright = MeanLuma(s);
        cs.ResetToDefault();
        double reset = MeanLuma(s);
        Report(bright > dark + 10, "manual exposure", string.Format(
            "luma auto {0:F0}, short exposure {1:F0}, long exposure {2:F0}, after reset {3:F0}", auto, dark, bright, reset));
    }

    // ---------------------------------------------------------------- tilt

    static void TestTilt(KinectSensor s)
    {
        int a0 = s.ElevationAngle;
        var acc0 = s.AccelerometerGetCurrentReading();
        int target = Math.Max(s.MinElevationAngle + 2, Math.Min(s.MaxElevationAngle - 2, a0 > 0 ? a0 - 6 : a0 + 6));
        Info("tilt", string.Format("current {0} deg, range {1}..{2}, target {3}", a0, s.MinElevationAngle, s.MaxElevationAngle, target));
        s.ElevationAngle = target;
        Thread.Sleep(2000);
        int a1 = s.ElevationAngle;
        var acc1 = s.AccelerometerGetCurrentReading();
        s.ElevationAngle = a0;
        Thread.Sleep(2000);
        int a2 = s.ElevationAngle;
        Report(Math.Abs(a1 - target) <= 2 && Math.Abs(a2 - a0) <= 2, "tilt motor", string.Format(
            "{0} deg -> set {1} -> {2} deg -> set {0} -> {3} deg (range {4}..{5})",
            a0, target, a1, a2, s.MinElevationAngle, s.MaxElevationAngle));
        Report(Math.Abs(acc0.Y) > 0.5, "accelerometer", string.Format(
            "({0:F2}, {1:F2}, {2:F2}) g at {3} deg, ({4:F2}, {5:F2}, {6:F2}) g at {7} deg",
            acc0.X, acc0.Y, acc0.Z, a0, acc1.X, acc1.Y, acc1.Z, a1));
    }

    // ---------------------------------------------------------------- audio

    static void TestAudio(KinectSensor s)
    {
        var a = s.AudioSource;
        try
        {
            using (var stream = a.Start())
            {
                var buf = new byte[32000 * 3]; // 3 s of 16 kHz 16-bit mono
                int got = 0;
                var sw = Stopwatch.StartNew();
                while (got < buf.Length && sw.Elapsed.TotalSeconds < 6)
                {
                    int n = stream.Read(buf, got, buf.Length - got);
                    if (n <= 0) { Thread.Sleep(10); continue; } // the stream returns 0 until data arrives
                    got += n;
                }
                double secs = sw.Elapsed.TotalSeconds;
                double sum = 0; int peak = 0;
                for (int i = 0; i + 1 < got; i += 2)
                {
                    int v = BitConverter.ToInt16(buf, i);
                    sum += (double)v * v;
                    peak = Math.Max(peak, Math.Abs(v));
                }
                double rms = got > 0 ? Math.Sqrt(sum / (got / 2)) : 0;
                Report(got == buf.Length && peak > 0, "audio capture", string.Format(
                    "{0} bytes in {1:F1} s, rms {2:F0}, peak {3}, beam {4:F0} deg, source {5:F0} deg (confidence {6:F2})",
                    got, secs, rms, peak, a.BeamAngle, a.SoundSourceAngle, a.SoundSourceAngleConfidence));
            }
        }
        catch (System.Runtime.InteropServices.COMException e)
        {
            // The SDK beamformer (KinectAudio10 DMO) needs Windows' mic-array support
            // (KSPROPERTY_AUDIO_MIC_ARRAY_GEOMETRY); the camera runtime is not involved.
            Report(false, "audio capture", string.Format("KinectAudioSource failed 0x{0:X8}: {1}", e.ErrorCode, e.Message));
        }
        finally
        {
            a.Stop();
        }
    }

    // ---------------------------------------------------------------- restart

    static void TestRestart(KinectSensor s)
    {
        for (int round = 1; round <= 3; round++)
        {
            s.Stop();
            Thread.Sleep(300);
            s.Start();
            var c = CaptureColor(s, 2.0);
            var d = CaptureDepth(s, 1.0);
            Report(c.Frames > 30 && d.Frames > 20, "stop/start #" + round,
                string.Format("color {0} frames {1:F1} fps, depth {2} frames", c.Frames, c.Fps, d.Frames));
        }
    }

    // ---------------------------------------------------------------- hotplug

    static int Hotplug(int seconds)
    {
        var log = new List<string>();
        var sw = Stopwatch.StartNew();
        bool sawDisconnect = false, sawReconnect = false;
        KinectSensor current = KinectSensor.KinectSensors.FirstOrDefault(k => k.Status == KinectStatus.Connected);
        Action<KinectSensor> start = k =>
        {
            k.ColorStream.Enable(ColorImageFormat.RgbResolution640x480Fps30);
            k.DepthStream.Enable(DepthImageFormat.Resolution320x240Fps30);
            k.Start();
        };
        if (current != null) start(current);
        Console.WriteLine("waiting {0} s; unplug the sensor, wait a few seconds, plug it back in", seconds);
        KinectSensor.KinectSensors.StatusChanged += (o, e) =>
        {
            Console.WriteLine("  {0,6:F1}s status {1} ({2})", sw.Elapsed.TotalSeconds, e.Status, e.Sensor.DeviceConnectionId);
            if (e.Status == KinectStatus.Disconnected) sawDisconnect = true;
            if (e.Status == KinectStatus.Connected && sawDisconnect) sawReconnect = true;
        };
        int colorFrames = 0;
        double lastReport = 0;
        while (sw.Elapsed.TotalSeconds < seconds)
        {
            var k = current;
            if (k == null || k.Status != KinectStatus.Connected || !k.IsRunning)
            {
                Thread.Sleep(200);
                var next = KinectSensor.KinectSensors.FirstOrDefault(x => x.Status == KinectStatus.Connected);
                if (next != null && (next != current || !next.IsRunning))
                {
                    try { start(next); current = next; Console.WriteLine("  {0,6:F1}s streams started", sw.Elapsed.TotalSeconds); }
                    catch (Exception ex) { Console.WriteLine("  start failed: {0}", ex.Message); }
                }
                continue;
            }
            try
            {
                using (var f = k.ColorStream.OpenNextFrame(200)) if (f != null) colorFrames++;
            }
            catch (Exception) { Thread.Sleep(200); }
            if (sw.Elapsed.TotalSeconds - lastReport >= 2)
            {
                lastReport = sw.Elapsed.TotalSeconds;
                Console.WriteLine("  {0,6:F1}s color frames {1}", lastReport, colorFrames);
                if (sawReconnect && colorFrames > 60) break;
                colorFrames = 0;
            }
        }
        if (current != null) current.Stop();
        Report(sawDisconnect && sawReconnect, "hotplug", string.Format("disconnect seen {0}, reconnect seen {1}", sawDisconnect, sawReconnect));
        return failures;
    }
}
