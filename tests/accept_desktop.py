"""Native desktop + installed CLI integration. No real accounts/provider calls."""
import argparse,json,os,socket,subprocess,tempfile,time,tomllib
from pathlib import Path

parser=argparse.ArgumentParser()
parser.add_argument('desktop',type=Path)
parser.add_argument('headless',type=Path)
parser.add_argument('--out',type=Path,required=True)
args=parser.parse_args();args.out.mkdir(parents=True,exist_ok=True)
desktop=args.desktop.resolve();headless=args.headless.resolve()
version=tomllib.loads((Path(__file__).resolve().parent.parent/'Cargo.toml').read_text(encoding='utf-8'))['package']['version']
identifier=json.loads((Path(__file__).resolve().parent.parent/'desktop/tauri.conf.json').read_text(encoding='utf-8'))['identifier']
checks=[]
def check(condition,name):
    assert condition,name
    checks.append(name)
def run(*cmd,**kwargs):return subprocess.check_output(list(map(str,cmd)),encoding='utf-8',**kwargs).strip()
def wait_for(function):
    until=time.monotonic()+40
    while time.monotonic()<until:
        try:value=function()
        except subprocess.CalledProcessError:value=None
        if value:return value
        time.sleep(.2)
    raise AssertionError('Native desktop acceptance timed out')
with tempfile.TemporaryDirectory(prefix='relay-native-') as temporary:
    root=Path(temporary);auth=root/'data';auth.mkdir()
    with socket.socket() as listener:listener.bind(('127.0.0.1',0));port=listener.getsockname()[1]
    env=dict(os.environ,RELAY_AUTH_DIR=str(auth),RELAY_PORT=str(port),RELAY_NO_BROWSER='1',
             RELAY_CODEX_VERSION='0.163.0',RELAY_MODEL_DISCOVERY='off',
             HTTPS_PROXY='http://127.0.0.1:1',HTTP_PROXY='http://127.0.0.1:1',NO_PROXY='127.0.0.1,localhost')
    env.pop('RELAY_API_KEY',None)
    def cli(*arguments):return json.loads(run(headless,*arguments,env=env,timeout=20))
    log=(args.out/'native-process.log').open('wb')
    process=subprocess.Popen([str(desktop),'--start-api'],env=env,stdout=log,stderr=log)
    try:
        wait_for(lambda:(auth/'control.json').is_file())
        current=cli('status')
        check(current['running'] and current['version']==version,'native desktop starts the current shared API')
        check(not any(a['configured'] for a in current['accounts']),'native management works without accounts')
        key=current['api_key']
        check(len(key)==47 and key.startswith('sk-'),'native and headless use the same persistent client key')
        duplicate=subprocess.run([str(desktop)],env=env,stdout=log,stderr=log,timeout=20)
        check(duplicate.returncode==0 and process.poll() is None,'second launch restores the original native instance')
        if os.uname().sysname=='Linux':
            windows=wait_for(lambda:run('xdotool','search','--onlyvisible','--pid',process.pid).splitlines())
            window=windows[-1]
            geometry=dict(line.split('=',1) for line in run('xdotool','getwindowgeometry','--shell',window).splitlines())
            width=int(geometry['WIDTH']);height=int(geometry['HEIGHT'])
            check(0<width<=1280 and 0<height<=900,'native window fits the test desktop working area')
            run('import','-window','root',args.out/'native-light.png')
            run('xdotool','windowactivate','--sync',window)
            # Click the actual frameless minimize button, then restore via executable.
            run('xdotool','mousemove','--window',window,width-105,19,'click','1')
            time.sleep(.6)
            check(cli('status')['running'],'API stays alive after native minimize')
            subprocess.run([str(desktop)],env=env,stdout=log,stderr=log,timeout=20,check=True)
            wait_for(lambda:'IsViewable' in run('xwininfo','-id',window))
            check(True,'second launch restores a minimized or tray-hidden window')
            patch=root/'appearance.json';patch.write_text(json.dumps({'appearance':{'theme':'dark','scale':115},'labels':{'primary':'Native QA'}}))
            cli('configure',patch);time.sleep(3)
            run('import','-window','root',args.out/'native-dark.png')
            run('xdotool','windowactivate','--sync',window,'key','--clearmodifiers','ctrl+q')
            check(process.wait(timeout=15)==0,'native Ctrl+Q quits the window and service')
        else:
            run('screencapture','-x',args.out/'native-light.png')
            # Standard Quit AppleEvent requires no Accessibility automation grant.
            run('osascript','-e',f'tell application id "{identifier}" to quit')
            check(process.wait(timeout=15)==0,'native application Quit stops the window and service')
        check(not (auth/'control.json').exists(),'native exit removes the control capability')
        check((auth/'api-key.txt').read_text().strip()==key,'native exit preserves the client key')
    finally:
        if process.poll() is None:process.terminate();process.wait(timeout=15)
        log.close()
(args.out/'native-acceptance.json').write_text(json.dumps(dict(passed=True,checks=checks),indent=2)+'\n')
print(json.dumps(dict(passed=True,checks=checks),indent=2))
