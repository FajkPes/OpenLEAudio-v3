using System;
using System.Collections.Generic;
using System.Text.Json;
using System.Threading.Tasks;
using Microsoft.UI.Dispatching;
using Windows.Media.Control;

namespace OpenLEAudio;

/// <summary>GMCS commands address the active Windows media session, never the
/// Bluetooth connection lifecycle. No disconnect handler pauses a player.</summary>
internal sealed class MediaBridge : IDisposable
{
    private readonly Func<string, Dictionary<string, object?>, Task<bool>> _send;
    private readonly Action<string> _log;
    private readonly DispatcherQueueTimer _timer;
    private GlobalSystemMediaTransportControlsSessionManager? _manager;
    private bool _refreshing, _disposed, _reportedUnavailable;

    public MediaBridge(DispatcherQueue queue, Func<string, Dictionary<string, object?>, Task<bool>> send, Action<string> log)
    {
        _send = send; _log = log;
        _timer = queue.CreateTimer(); _timer.Interval = TimeSpan.FromSeconds(1);
        _timer.Tick += async (_, _) => await RefreshAsync();
        _timer.Start();
        _ = RefreshAsync();
    }
    private async Task<GlobalSystemMediaTransportControlsSession?> CurrentAsync()
    {
        _manager ??= await GlobalSystemMediaTransportControlsSessionManager.RequestAsync();
        return _manager.GetCurrentSession();
    }
    private static int Centiseconds(TimeSpan time) => (int)Math.Clamp(time.TotalMilliseconds / 10, 0, int.MaxValue);

    public async Task RefreshAsync()
    {
        if (_disposed || _refreshing) return;
        _refreshing = true;
        try
        {
            var session = await CurrentAsync();
            var data = new Dictionary<string, object?> { ["player"] = "Windows", ["title"] = "", ["state"] = 0,
                ["duration"] = -1, ["position"] = 0, ["supported"] = 0 };
            if (session is not null)
            {
                var playback = session.GetPlaybackInfo();
                var controls = playback.Controls;
                var props = await session.TryGetMediaPropertiesAsync();
                var timeline = session.GetTimelineProperties();
                var mask = (controls.IsPlayEnabled ? 1 : 0) | (controls.IsPauseEnabled ? 2 : 0)
                    | (controls.IsStopEnabled ? 16 : 0) | (controls.IsPreviousEnabled ? 0x800 : 0)
                    | (controls.IsNextEnabled ? 0x1000 : 0);
                data["player"] = session.SourceAppUserModelId;
                data["title"] = props?.Title ?? "";
                data["state"] = playback.PlaybackStatus switch {
                    GlobalSystemMediaTransportControlsSessionPlaybackStatus.Playing => 1,
                    GlobalSystemMediaTransportControlsSessionPlaybackStatus.Paused => 2,
                    GlobalSystemMediaTransportControlsSessionPlaybackStatus.Stopped => 2,
                    _ => 0,
                };
                data["duration"] = timeline.EndTime > timeline.StartTime ? Centiseconds(timeline.EndTime - timeline.StartTime) : -1;
                data["position"] = Centiseconds(timeline.Position - timeline.StartTime);
                data["supported"] = mask;
            }
            if (!_disposed) await _send("media-status", data);
        }
        catch (Exception error)
        {
            if (!_reportedUnavailable) { _reportedUnavailable = true; _log($"Windows media integration unavailable: {error.Message}"); }
            if (!_disposed) await _send("media-status", new() { ["state"] = 0, ["supported"] = 0 });
        }
        finally { _refreshing = false; }
    }
    public async Task ExecuteAsync(JsonElement message)
    {
        if (_disposed) return;
        var id = message.GetProperty("id").GetUInt64();
        var opcode = message.GetProperty("opcode").GetInt32();
        var ok = false;
        try
        {
            var session = await CurrentAsync();
            if (session is not null)
            {
                switch (opcode)
                {
                    case 1: ok = await session.TryPlayAsync(); break;
                    case 2: ok = await session.TryPauseAsync(); break;
                    case 5: ok = await session.TryStopAsync(); break;
                    case 0x30: ok = await session.TrySkipPreviousAsync(); break;
                    case 0x31: ok = await session.TrySkipNextAsync(); break;
                    case 0:
                        if (message.TryGetProperty("position", out var value) && value.ValueKind == JsonValueKind.Number)
                        {
                            var timeline = session.GetTimelineProperties();
                            var wanted = TimeSpan.FromMilliseconds(value.GetInt32() * 10.0);
                            var absolute = wanted < TimeSpan.Zero ? timeline.EndTime + wanted : timeline.StartTime + wanted;
                            var ticks = Math.Clamp(absolute.Ticks, timeline.StartTime.Ticks, Math.Max(timeline.StartTime.Ticks, timeline.EndTime.Ticks));
                            ok = await session.TryChangePlaybackPositionAsync(ticks);
                        }
                        break;
                }
            }
        }
        catch (Exception error) { _log($"Headphone media command failed: {error.Message}"); }
        if (!_disposed)
        {
            await _send("media-result", new() { ["id"] = id, ["success"] = ok });
            _log($"Headphone media command 0x{opcode:X2}: {(ok ? "accepted by Windows" : "not completed by Windows")}");
            await RefreshAsync();
        }
    }
    public void Dispose() { _disposed = true; _timer.Stop(); }
}
