// Benchmark-only facade. Production GlissClient and AudioPlayer are unchanged.
(() => {
  const Native = window.WebSocket;
  class SplitSocket extends EventTarget {
    constructor(url,protocols) {
      super();
      this.main = protocols === undefined ? new Native(url) : new Native(url,protocols);
      const companion = new URL(url);companion.pathname='/audio';companion.search='';
      this.companion = new Native(companion);this.closing=false;
      for (const socket of [this.main,this.companion]) {
        socket.binaryType='arraybuffer';
        socket.addEventListener('open',() => {
          if (this.readyState === Native.OPEN) this.emit('open',new Event('open'));
        });
        socket.addEventListener('message',event => this.emit('message',new MessageEvent('message',{data:event.data,origin:event.origin})));
        socket.addEventListener('error',() => this.emit('error',new Event('error')));
        socket.addEventListener('close',event => {
          if (this.closing) return;this.closing=true;
          this.main.close();this.companion.close();
          this.emit('close',new CloseEvent('close',{code:event.code,reason:event.reason,wasClean:event.wasClean}));
        });
      }
    }
    emit(type,event) {this.dispatchEvent(event);this['on'+type]?.(event);}
    get readyState() {
      if (this.closing) return Native.CLOSING;
      return this.main.readyState === Native.OPEN && this.companion.readyState === Native.OPEN ? Native.OPEN : Native.CONNECTING;
    }
    get bufferedAmount() {return this.main.bufferedAmount+this.companion.bufferedAmount;}
    get binaryType() {return this.main.binaryType;}
    set binaryType(value) {this.main.binaryType=value;this.companion.binaryType=value;}
    send(data) {
      let audio=false;
      if (typeof data === 'string') {try {audio=JSON.parse(data).type === 'audio';} catch {}}
      (audio && window.glissIsolationMode === 'split' ? this.companion : this.main).send(data);
    }
    close(code,reason) {this.main.close(code,reason);this.companion.close(code,reason);}
  }
  window.WebSocket = new Proxy(Native,{construct(Target,args) {
    if (new URL(args[0],location.href).pathname === '/ws') return new SplitSocket(...args);
    return new Target(...args);
  }});
})();
