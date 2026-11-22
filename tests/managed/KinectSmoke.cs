// Smoke test for managed (Microsoft.Kinect.dll) applications.
// Build: csc /platform:x64 /r:Microsoft.Kinect.dll KinectSmoke.cs, then put the
// proxy kinect10.dll next to KinectSmoke.exe.
using System;
using System.Linq;
using System.Threading;
using Microsoft.Kinect;

static class KinectSmoke
{
    static int Main(string[] args)
    {
        int seconds = args.Length > 0 ? int.Parse(args[0]) : 8;
        Console.WriteLine("process: {0}-bit", IntPtr.Size * 8);
        var sensor = KinectSensor.KinectSensors.FirstOrDefault();
        if (sensor == null) { Console.WriteLine("no sensor"); return 1; }
        Console.WriteLine("sensor {0} status {1}", sensor.UniqueKinectId, sensor.Status);
        int color = 0, depth = 0, skel = 0, tracked = 0;
        sensor.ColorStream.Enable(ColorImageFormat.RgbResolution640x480Fps30);
        sensor.DepthStream.Enable(DepthImageFormat.Resolution640x480Fps30);
        sensor.SkeletonStream.Enable();
        var skeletons = new Skeleton[6];
        sensor.ColorFrameReady += (s, e) => { using (var f = e.OpenColorImageFrame()) if (f != null) color++; };
        sensor.DepthFrameReady += (s, e) => { using (var f = e.OpenDepthImageFrame()) if (f != null) depth++; };
        sensor.SkeletonFrameReady += (s, e) =>
        {
            using (var f = e.OpenSkeletonFrame())
            {
                if (f == null) return;
                skel++;
                f.CopySkeletonDataTo(skeletons);
                tracked = Math.Max(tracked, skeletons.Count(k => k.TrackingState == SkeletonTrackingState.Tracked));
            }
        };
        sensor.Start();
        Console.WriteLine("started; elevation {0} deg", sensor.ElevationAngle);
        Thread.Sleep(seconds * 1000);
        sensor.Stop();
        Console.WriteLine("color {0} ({1:F1} fps) depth {2} ({3:F1} fps) skeleton {4} (max tracked {5})",
            color, color / (double)seconds, depth, depth / (double)seconds, skel, tracked);
        return color > 0 && depth > 0 ? 0 : 1;
    }
}
