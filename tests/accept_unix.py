"""Installed Unix paths, private state, Finder-like PATH and graceful SIGTERM."""
import json,os,signal,socket,subprocess,sys,tempfile,time
from pathlib import Path

binary=Path(sys.argv[1]).resolve()
checks=[]
def check(condition,name):
    assert condition,name
    checks.append(name)
def wait_for(function):
    until=time.monotonic()+25
    while time.monotonic()<until:
        result=function()
        if result:return result
        time.sleep(.1)
    raise AssertionError('Unix acceptance timed out')
with tempfile.TemporaryDirectory(prefix='relay-unix-') as temporary:
    root=Path(temporary);home=root/'home';home.mkdir()
    env=dict(os.environ,HOME=str(home),PATH='/usr/bin:/bin',RELAY_NO_BROWSER='1',
             RELAY_CODEX_VERSION='0.163.0',RELAY_MODEL_DISCOVERY='off',
             HTTPS_PROXY='http://127.0.0.1:1',HTTP_PROXY='http://127.0.0.1:1',NO_PROXY='127.0.0.1,localhost')
    for name in ('RELAY_AUTH_DIR','RELAY_LOCAL','RELAY_PYTHON','RELAY_API_KEY'):env.pop(name,None)
    if sys.platform=='darwin':expected=home/'Library/Application Support/praxis-relay'
    else:
        env['XDG_DATA_HOME']=str(root/'xdg');expected=root/'xdg/praxis-relay'
    def cli(*args):
        completed=subprocess.run([str(binary),*args],env=env,capture_output=True,text=True,encoding='utf-8',timeout=35)
        assert completed.returncode==0,(args,completed.stderr)
        return json.loads(completed.stdout)
    status=cli('status')
    check(Path(status['auth_directory'])==expected,'installed binary uses per-user platform storage')
    check(expected.stat().st_mode&0o777==0o700,'data root is private 0700')
    check((expected/'api-key.txt').stat().st_mode&0o777==0o600,'client key is private 0600')
    with socket.socket() as listener:listener.bind(('127.0.0.1',0));port=listener.getsockname()[1]
    env['RELAY_PORT']=str(port)
    with (root/'process.log').open('wb') as log:
        process=subprocess.Popen([str(binary),'serve','--headless'],env=env,stdout=log,stderr=log)
        try:
            wait_for(lambda:(expected/'control.json').is_file())
            check((expected/'control.json').stat().st_mode&0o777==0o600,'control capability is private 0600')
            cli('login','primary')
            wait_for(lambda:cli('status')['login_url'])
            check(cli('status')['login']['pending'],'browser helper starts with a desktop-launch PATH')
            cli('cancel_login')
            check(not cli('status')['login']['pending'],'desktop-launch login cancels cleanly')
            process.send_signal(signal.SIGTERM)
            check(process.wait(timeout=15)==0,'SIGTERM exits headless successfully')
            check(not (expected/'control.json').exists(),'SIGTERM removes its control capability')
            check(cli('status')['api_key']==status['api_key'],'SIGTERM preserves the persistent client key')
        finally:
            if process.poll() is None:process.terminate();process.wait(timeout=15)
print(json.dumps(dict(passed=True,checks=checks),indent=2))
