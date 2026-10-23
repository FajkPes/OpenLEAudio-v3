using System;
using System.IO;
using System.IO.Pipes;
using System.Linq;
using System.Text;
using System.Text.Json;
using System.Threading;
using System.Threading.Tasks;
using System.Collections.Generic;
namespace OpenLEAudio;
public sealed partial class MainWindow
{
    private CancellationTokenSource? _controllerStop;
    private string _controllerAdapter="";
    private int? _controllerSignal;
    private void StartControllerBridge()
    {
        _controllerStop=new CancellationTokenSource();var token=_controllerStop.Token;
        Closed+=(_,_)=>{_controllerStop.Cancel();};
        _=Task.Run(async()=>{
            while(!token.IsCancellationRequested){
                try{
                    using var pipe=new NamedPipeServerStream("OpenLEAudio4.Controller.v1",PipeDirection.InOut,1,PipeTransmissionMode.Byte,PipeOptions.Asynchronous|PipeOptions.CurrentUserOnly);
                    await pipe.WaitForConnectionAsync(token);
                    using var timeout=CancellationTokenSource.CreateLinkedTokenSource(token);timeout.CancelAfter(4000);
                    byte[] data=new byte[4096];int count=0;
                    while(count<data.Length){int n=await pipe.ReadAsync(data.AsMemory(count,1),timeout.Token);if(n==0)break;if(data[count++] == 10)break;}
                    if(count==0||count>=data.Length)continue;
                    using var request=JsonDocument.Parse(data.AsMemory(0,count));var req=request.RootElement.Clone();
                    var completion=new TaskCompletionSource<object>(TaskCreationOptions.RunContinuationsAsynchronously);
                    if(!_ui.TryEnqueue(()=>{try{completion.SetResult(ControllerRequest(req));}catch(Exception e){completion.SetResult(new{ok=false,error=e.Message});}}))continue;
                    var response=await completion.Task.WaitAsync(timeout.Token);
                    byte[] bytes=Encoding.UTF8.GetBytes(JsonSerializer.Serialize(response)+"\n");await pipe.WriteAsync(bytes,timeout.Token);await pipe.FlushAsync(timeout.Token);
                }catch(OperationCanceledException){}catch{if(!token.IsCancellationRequested)await Task.Delay(500,token);}
            }
        },token);
    }
    private object ControllerRequest(JsonElement req)
    {
        string cmd=req.GetProperty("cmd").GetString()??"";
        if(cmd=="state"){
            var current=_paired.Concat(_found).FirstOrDefault(d=>AddressesMatch(d.Address,_connectedAddress??""));
            var favorite=_paired.Concat(_found).Where(d=>FavoriteDevices.Contains(d.Address)).GroupBy(d=>FavoriteDevices.Key(d.Address)).Select(g=>g.OrderByDescending(d=>d.Paired).First()).Select(d=>new{address=d.Address,name=d.Name,paired=d.Paired,connected=AddressesMatch(d.Address,_connectedAddress??"")}).ToArray();
            return new{ok=true,adapter=_adapterOn,adapterName=_controllerAdapter,connected=!string.IsNullOrEmpty(_connectedAddress),device=current?.Name??_connectedAddress??"",address=_connectedAddress,connectedSince=_connectedSince,battery=_connectedAddress==null?Array.Empty<int>():_batteryLevels.ToArray(),rssi=_connectedAddress==null?null:_controllerSignal,favorites=favorite,busy=Busy.IsActive};
        }
        if(_agent==null)throw new InvalidOperationException("Audio core is unavailable.");
        if(Busy.IsActive)throw new InvalidOperationException("OpenLEAudio is busy; wait for the current operation.");
        if(cmd=="adapter"){
            bool on=req.GetProperty("on").GetBoolean();StopStartupReconnect();Busy.IsActive=true;
            if(on)Send("check");Send("adapter",new(){["on"]=on});
        }else if(cmd is "connect" or "disconnect"){
            string address=req.GetProperty("address").GetString()??"";
            var device=_paired.Concat(_found).FirstOrDefault(d=>d.Paired&&AddressesMatch(d.Address,address));
            bool current=AddressesMatch(address,_connectedAddress??"");
            if(cmd=="connect"&&(device==null||!FavoriteDevices.Contains(device.Address)))throw new InvalidOperationException("Only paired favorite headphones can be connected.");
            if(cmd=="disconnect"&&!current)throw new InvalidOperationException("These headphones are no longer connected.");
            StopStartupReconnect();
            if(cmd=="connect"){
                if(!_adapterOn)throw new InvalidOperationException("Turn on the OpenLEAudio adapter first.");
                if(!string.IsNullOrEmpty(_connectedAddress)&&!AddressesMatch(address,_connectedAddress))throw new InvalidOperationException("Disconnect the current headphones first.");
                if(!AddressesMatch(address,_connectedAddress??"")){Busy.IsActive=true;Send("connect",new(){["address"]=device!.Address,["confirmed"]=true});}
            }else if(AddressesMatch(address,_connectedAddress??"")){Busy.IsActive=true;Send("disconnect");}
        }else throw new InvalidOperationException("Unsupported controller command.");
        return new{ok=true,accepted=true};
    }
}
