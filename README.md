# Socksidizer

Warning: this is a prototype!

## Overview

**Summary:**
a [SOCKS5](https://datatracker.ietf.org/doc/html/rfc1928) proxy
which listens on a [Unix domain socket](https://man7.org/linux/man-pages/man7/unix.7.html) (UDS)
and connects to Unix domain sockets.

![Screenshot](doc/screenshot-commented.png)

Example protocol stack:

<pre>
[HTTP ]<----------------->[HTTP ]
[SOCKS]<-->[SOCKS]        [-----]
[UDS  ]<-->[UDS  ]<------>[UDS  ]
Firefox    Socksidizer    Local Service
</pre>

**Why:**
the main motivation is to conveniently serve local (user-scoped) web applications:

* access them by application name (`http://myapp.foo.local`);
* without managing IP addresses and TCP ports;
* on Linux, rely on file filesystem permissions to prevent other user from accessing the service.

### Features

Current features:

* listen on a Unix domain socket;
* SOCKS5 protocol;
* SOCKS addressing using domain name (aka `socks5h`);
* SOCKS5 CONNECT method (used for TCP connections);
* connect to (`AF_UNIX` `SOCK_STREAM`) Unix domain sockets (of the form `{directory}/{hostname}_{port}`).

Potential upcoming features:

* listen on TCP sockets;
* support for socket activation;
* more flexible configuration;
* support for connecting to the requested service using a shell command (similar to OpenSSH `ProxyCommand`).

Features which probably won't be implemented:

* SOCKS authentication (is that useful?);
* UDP ASSOCIATE support;
* SOCKS addressing using IP address.

### Explanations

This works like this:

1. socksidizer accepts connection on some UDS socks (eg. `/run/user/${PID}/socksidizer.socks`);
2. socksidizer receives a SOCKS5 proxy (CONNECT) request from the client;
3. socksidizer translates this request into a pathname of the form `{directory}/{hostname}_{port}` (eg. `/run/user/${PID}/published/myapp.john.local_443`) and connects to this socket;
4. socksidizer relays data between the client and the service.

This currently requires a client which supports SOCKS5 over UDS.
Firefox and its derivatives support this.
Other browsers currently do not support this as far as I known.


## Build

~~~sh
cargo build
~~~

Build and execute:

~~~sh
cargo run -- --socket "${XDG_RUNTIME_DIR}/socksidizer.socks" --directory "${XDG_RUNTIME_DIR}/publish"
~~~


## Execution

~~~sh
(umask 077 ; mkdir -p "${XDG_RUNTIME_DIR}/publish" && chmod 700 ${XDG_RUNTIME_DIR}/publish)
socksidizer --socket "${XDG_RUNTIME_DIR}/socksidizer.socks" --directory "${XDG_RUNTIME_DIR}/publish"
~~~

Warnings:

* Even if the service is only *directly* available to the system user,
  a malicious website could still attempt to attack it by exploiting your browser (CSRF attacks).
* You must use a dedicated directory storing the sockets of the published services.
  Do **not** use `${XDG_RUNTIME_DIR}` (i.e. `/var/run/${PID}`) or any other directory
  which has unrelated UDS.
* Depending on the permissions on the Unix domain sockets and the parents directories,
  services can be reachable (either directly of through the SOCKS proxy).
  On Linux systems,
  the SOCKS Unix domain socket created by socksidizer is only reachable by the user by default.
  Moreover, by default, Socksidizer checks that UDS connection comes from the same user
  and immediately closes the connection otherwise.

On Linux, the following makes sure that the `"${XDG_RUNTIME_DIR}/publish"` directory
is only reachable by the user:

~~~sh
(umask 077 ; mkdir -p "${XDG_RUNTIME_DIR}/publish" && chmod 700 ${XDG_RUNTIME_DIR}/publish)
~~~

Warning: `umask 077 && my_command` does not work.


## Client configuration

### Firefox

You can configure Firefox to use a UDS SOCKS proxy.
In the Network configuration:,

* usa a value of the form `file:///run/user/${PID}/socksidizer.socks` in SOCKS proxy;
* choose SOCKS5;
* the port is ignored.

However, this approach is not very usable.
All the requests are going to go through the SOCKS proxy
which currently does not support proxying to TCP/UDP.
Only your web sites will be handled by the proxy.
The solution is to install FoxyProxy (or a similar extension).

Note: it is not possible to configure UDS proxy using Proxy Auto-Configuration (`proxy.pac`).

### Firefox with FoxyProxy

[FoxyProxy](https://addons.mozilla.org/en-US/firefox/addon/foxyproxy-standard/)
lets you define different network configurations depending on the target URI
i.e. you can use your default network configuretion (eg. no proxy) for most URIS
but use Socksidizer to reach some URIS (eg. `http://*.foo.localhost`).

Add a new proxy in FoxyProxy configuration:

* choose "SOCKS5" for the proxy type;
* enter an address of the form `file:///run/user/${PID}/socksidizer.socks`;
* enter any value for the port (it is ignored);
* check "Send DNS through SOCKS5 proxy";
* enable it.

Add a pattern for this proxy:

* `*.foo.localhost`
* wildcard;
* "http";
* enabled.

Select "use proxys by template".


## Service configuration

### Socket activation

If your application supports socket-activation, you can use:

~~~sh
systemd-socket-activate -l "${XDG_RUNTIME_DIR}/publish/app.foo.local" ./myapp
~~~

### SSH relay

You can expose an application over a SSH tunnel.
If the remote service is available over TCP:

~~~sh
ssh target -N -L "${XDG_RUNTIME_DIR}/publish/app.foo.local_80:localhost:80"
~~~

If the remote sevice is available over UDS:

~~~sh
ssh target -N -L "${XDG_RUNTIME_DIR}/publish/app.foo.local_80:/run/foo.sock
~~~

### CURL

CURL can use a UDS SOCKS proxy with a proxy URI of the form:

~~~
socks5h://localhost/run/user/1000/socksidizer.socks
~~~

Example:

~~~sh
curl -x socks5h://localhost/run/user/1000/socksidizer.socks http://app.foo.local
~~~

### Podman

If your application is in a rootless (i.e. user) [Podman](https://podman.io/) container,
you can access it even if is not published by Podman:

~~~sh
pid="$(podman inspect $container_name -f '{{.State.Pid}}')"
nsenter -t "$pid" -U -n socat UNIX-LISTEN:${XDG_RUNTIME_DIR}/publish/app.foo.local_80,mode=mode=700,fork TCP:127.0.0.1:8000
~~~

### Flask

Using the `flask` command:

~~~sh
flask run --host=unix://${XDG_RUNTIME_DIR}/publish/app.foo.local_80
~~~

From Python:

~~~python
app.run(host="unix://" + os.environ["XDG_RUNTIME_DIR"] + "/publish/app.foo.local_80")
~~~

### Python WSGI applications

When served using gunicorn:

~~~sh
gunicorn --bind unix:${XDG_RUNTIME_DIR}/publish/app.foo.local_80 api:app
~~~

### Node.js

~~~js
server.listen(process.env.process.env.XDG_RUNTIME_DIR + "/publish/app.foo.local_80")
~~~


## FAQ

**Can I do the same thing with Chrome (or another browser)?**

As far as I understand, Firefox (and its derivative) is the only browser
which can talk to a SOCKS proxy over UDS. It would be possible to have
Socksidizer listen on TCP localhost. This could be useful
but other local users would still be able to connect to the SOCKS proxy.


## References

* [Chromium feature request - Support HTTP over Unix Sockets](https://bugs.chromium.org/p/chromium/issues/detail?id=451721)
* [Firefox feature request - Support HTTP over unix domain sockets](https://bugzilla.mozilla.org/show_bug.cgi?id=1688774)
* [WHATWG feature quest - Addressing HTTP servers over Unix domain sockets](https://github.com/whatwg/url/issues/577)
