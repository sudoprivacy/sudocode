/** Windows 10+ attaches the unnamed Job in CreateProcess itself. The native
 * program cannot run outside it, even if the keeper exits during startup. */
export const windowsProxyJobSource = String.raw`
$ErrorActionPreference='Stop'
$ProgressPreference='SilentlyContinue'
Microsoft.PowerShell.Core\Import-Module -Name "$PSHOME\Modules\Microsoft.PowerShell.Utility\Microsoft.PowerShell.Utility.psd1"
Microsoft.PowerShell.Utility\Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.IO.Pipes;
using System.Text;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Threading;
using Microsoft.Win32.SafeHandles;

public static class CodexProxyJob {
    [StructLayout(LayoutKind.Sequential)] struct Limits {
        public long ProcessTime, JobTime; public uint Flags;
        public UIntPtr MinWorkingSet, MaxWorkingSet; public uint ActiveLimit;
        public UIntPtr Affinity; public uint Priority, Scheduling;
        public ulong ReadOps, WriteOps, OtherOps, ReadBytes, WriteBytes, OtherBytes;
        public UIntPtr ProcessMemory, JobMemory, PeakProcessMemory, PeakJobMemory;
    }
    [StructLayout(LayoutKind.Sequential)] struct Accounting {
        public long User, Kernel, PeriodUser, PeriodKernel;
        public uint Faults, Total, Active, Terminated;
    }
    [StructLayout(LayoutKind.Sequential)] struct Startup {
        public int Size; public IntPtr Reserved, Desktop, Title;
        public int X,Y,Width,Height,Cols,Rows,Fill,Flags;
        public short Show,ReservedSize; public IntPtr ReservedBytes,Input,Output,Error,Attributes;
    }
    [StructLayout(LayoutKind.Sequential)] struct ProcessInfo {
        public IntPtr Process, Thread; public int Pid, Tid;
    }
    [DllImport("kernel32",SetLastError=true)] static extern IntPtr CreateJobObjectW(IntPtr security,IntPtr name);
    [DllImport("kernel32",SetLastError=true)] static extern bool SetInformationJobObject(IntPtr job,int type,ref Limits value,int size);
    [DllImport("kernel32",SetLastError=true)] static extern bool QueryInformationJobObject(IntPtr job,int type,out Accounting info,int size,IntPtr used);
    [DllImport("kernel32",SetLastError=true)] static extern bool TerminateJobObject(IntPtr job,uint code);
    [DllImport("kernel32",SetLastError=true)] static extern bool InitializeProcThreadAttributeList(IntPtr list,int count,int flags,ref IntPtr size);
    [DllImport("kernel32",SetLastError=true)] static extern bool UpdateProcThreadAttribute(IntPtr list,int flags,IntPtr key,IntPtr value,IntPtr size,IntPtr old,IntPtr used);
    [DllImport("kernel32")] static extern void DeleteProcThreadAttributeList(IntPtr list);
    [DllImport("kernel32")] static extern IntPtr GetStdHandle(int stream);
    [DllImport("kernel32",SetLastError=true)] static extern bool SetHandleInformation(IntPtr handle,int mask,int flags);
    [DllImport("kernel32",CharSet=CharSet.Unicode,SetLastError=true)] static extern bool CreateProcessW(string exe,StringBuilder cmd,IntPtr ps,IntPtr ts,bool inherit,int flags,byte[] env,string cwd,ref Startup startup,out ProcessInfo info);
    [DllImport("kernel32",SetLastError=true)] static extern bool GetExitCodeProcess(IntPtr process,out int code);
    [DllImport("kernel32")] static extern bool CloseHandle(IntPtr handle);
    const int HandleList = 0x20002, JobList = 0x2000d;
    const int ExtendedStartupInfoPresent = 0x80000, CreateNoWindow = 0x08000000, UnicodeEnvironment = 0x400;
    const int UseStdHandles = 0x100, HandleFlagInherit = 1;
    const int JobExtendedLimits = 9, JobBasicAccounting = 1, KillOnJobClose = 0x2000;

    static void Check(bool ok) {
        if (!ok) throw new Win32Exception(Marshal.GetLastWin32Error());
    }

    static ProcessInfo Launch(IntPtr job, string exe, string cmd, byte[] env) {
        var startup = new Startup {
            Size = Marshal.SizeOf(typeof(Startup)), Flags = UseStdHandles,
            Input = GetStdHandle(-10), Output = GetStdHandle(-11), Error = GetStdHandle(-12)
        };
        // The first call supplies the required buffer size.
        IntPtr size = IntPtr.Zero;
        InitializeProcThreadAttributeList(IntPtr.Zero, 2, 0, ref size);
        startup.Attributes = Marshal.AllocHGlobal(size);
        var handles = Marshal.AllocHGlobal(4 * IntPtr.Size);
        bool initialized = false;
        try {
            Check(InitializeProcThreadAttributeList(startup.Attributes, 2, 0, ref size));
            initialized = true;
            var stdio = new[] {startup.Input, startup.Output, startup.Error};
            for (int i = 0; i < stdio.Length; i++) {
                Check(SetHandleInformation(stdio[i], HandleFlagInherit, HandleFlagInherit));
                Marshal.WriteIntPtr(handles, i * IntPtr.Size, stdio[i]);
            }
            var jobSlot = IntPtr.Add(handles, 3 * IntPtr.Size);
            Marshal.WriteIntPtr(jobSlot, job);
            Check(UpdateProcThreadAttribute(startup.Attributes, 0, (IntPtr)HandleList, handles,
                (IntPtr)(3 * IntPtr.Size), IntPtr.Zero, IntPtr.Zero));
            Check(UpdateProcThreadAttribute(startup.Attributes, 0, (IntPtr)JobList, jobSlot,
                (IntPtr)IntPtr.Size, IntPtr.Zero, IntPtr.Zero));
            ProcessInfo info;
            Check(CreateProcessW(exe, new StringBuilder(cmd), IntPtr.Zero, IntPtr.Zero, true,
                CreateNoWindow | ExtendedStartupInfoPresent | UnicodeEnvironment, env, null, ref startup, out info));
            return info;
        } finally {
            if (initialized) DeleteProcThreadAttributeList(startup.Attributes);
            Marshal.FreeHGlobal(handles);
            Marshal.FreeHGlobal(startup.Attributes);
        }
    }

    public static int Run(string exe, string cmd, byte[] env, NamedPipeClientStream control, StreamWriter reply) {
        IntPtr job = CreateJobObjectW(IntPtr.Zero, IntPtr.Zero);
        if (job == IntPtr.Zero) {
            var error = new Win32Exception(Marshal.GetLastWin32Error());
            reply.WriteLine("drained"); // No process was created.
            throw error;
        }
        var info = new ProcessInfo();
        int exitCode = 1;
        try {
            var limits = new Limits {Flags = KillOnJobClose};
            Check(SetInformationJobObject(job, JobExtendedLimits, ref limits, Marshal.SizeOf(typeof(Limits))));
            info = Launch(job, exe, cmd, env);
            CloseHandle(info.Thread);
            reply.WriteLine(info.Pid);
            using (var native = new ManualResetEvent(false)) {
                native.SafeWaitHandle = new SafeWaitHandle(info.Process, false);
                var stop = control.BeginRead(new byte[1], 0, 1, null, null);
                WaitHandle.WaitAny(new[] {native, stop.AsyncWaitHandle});
            }
        } finally {
            try {
                if (info.Process != IntPtr.Zero) {
                    Check(TerminateJobObject(job, 1));
                    Accounting accounting;
                    do {
                        Check(QueryInformationJobObject(job, JobBasicAccounting, out accounting,
                            Marshal.SizeOf(typeof(Accounting)), IntPtr.Zero));
                        if (accounting.Active != 0) Thread.Sleep(10);
                    } while (accounting.Active != 0);
                    Check(GetExitCodeProcess(info.Process, out exitCode));
                }
                reply.WriteLine("drained");
            } finally {
                CloseHandle(job);
                if (info.Process != IntPtr.Zero) CloseHandle(info.Process);
            }
        }
        return exitCode;
    }
}
'@
function Start-ProxyJob($name,$token) {
    $pipe=[IO.Pipes.NamedPipeClientStream]::new('.',$name,[IO.Pipes.PipeDirection]::InOut,[IO.Pipes.PipeOptions]::Asynchronous)
    try {
        $pipe.Connect(10000)
        $utf8=[Text.UTF8Encoding]::new($false)
        $reader=[IO.StreamReader]::new($pipe,$utf8)
        $reply=[IO.StreamWriter]::new($pipe,$utf8);$reply.AutoFlush=$true
        $reply.WriteLine($token)
        $request=$reader.ReadLine() | Microsoft.PowerShell.Utility\ConvertFrom-Json
        $exitCode=[CodexProxyJob]::Run($request.executable,$request.command,[Convert]::FromBase64String($request.environment),$pipe,$reply)
        exit $exitCode
    } catch {
        if($reply){try{$reply.WriteLine('error '+$_.Exception.GetBaseException().Message)}catch{}}
        exit 1
    } finally {$pipe.Dispose()}
}
`;
