# G4 helper: holds a viewer's PTY from outside and times what comes out of it (perl-base only:
# no IO::Pty, no Time::HiRes). The PTY is opened through /dev/ptmx with ioctl; the clock is
# clock_gettime(CLOCK_MONOTONIC) through syscall(2), nanosecond resolution (x86_64, aarch64).
# Modes (all print `KEY value ...` lines; times are milliseconds):
#   now                                  monotonic clock, ms
#   session SOCK FILE                    open a windowed session `sh -c <contents of FILE>` on the daemon at SOCK
#                                        through the wire (a window connection); prints `pid N`; holds until stdin EOF
#   first MARKER TIMEOUT CMD...          time from spawning CMD on a PTY to MARKER in its output
#   exit TIMEOUT CMD...                  time from spawning CMD on a PTY to its exit
#   echo N READY CMD...                  N keystrokes written to the PTY, each timed to its echo (READY `-`: no wait)
#   bytes READY SECS CMD...              bytes on the PTY output while a workload runs (see g4.sh)
#   stall N SECS READY FLOODEND CMD...   two viewers of one session; viewer A stops reading for SECS under a flood
use strict;
use warnings;
use Fcntl;
use POSIX ();
use Socket;

my $SYS = { x86_64 => 228, aarch64 => 113 }->{ (POSIX::uname())[4] }
  or die "g4pty: unsupported architecture for clock_gettime\n";

sub now {
    my $b = "\0" x 16;
    syscall( $SYS, 1, $b ) == 0 or die "g4pty: clock_gettime: $!\n";
    my ( $s, $n ) = unpack( 'q q', $b );
    return $s * 1000 + $n / 1e6;
}

sub spawn {
    my @cmd = @_;
    sysopen( my $m, '/dev/ptmx', O_RDWR | O_NOCTTY ) or die "g4pty: /dev/ptmx: $!\n";
    my $z = pack( "i", 0 );
    ioctl( $m, 0x40045431, $z ) or die "g4pty: unlock pty: $!\n";
    my $nb = pack( 'i', 0 );
    ioctl( $m, 0x80045430, $nb ) or die "g4pty: ptsname: $!\n";
    my $n = unpack( 'i', $nb );
    my $ws = pack( "S4", 24, 80, 0, 0 );
    ioctl( $m, 0x5414, $ws );
    my $pid = fork();
    die "g4pty: fork: $!\n" unless defined $pid;
    if ( !$pid ) {
        POSIX::setsid();
        sysopen( my $s, "/dev/pts/$n", O_RDWR ) or die "g4pty: slave: $!\n";
        ioctl( $s, 0x540E, 0 );
        open( STDIN,  '<&', $s ) or die;
        open( STDOUT, '>&', $s ) or die;
        open( STDERR, '>&', $s ) or die;
        close $m;
        $ENV{TERM} ||= 'xterm-256color';
        exec @cmd or die "g4pty: exec $cmd[0]: $!\n";
    }
    return { m => $m, pid => $pid, buf => '' };
}

# One read with a timeout in seconds: the data, undef on timeout, '' at EOF.
sub rd {
    my ( $h, $to ) = @_;
    my $r = '';
    vec( $r, fileno( $h->{m} ), 1 ) = 1;
    my $n = select( my $o = $r, undef, undef, $to );
    return undef unless $n && $n > 0;
    my $d;
    my $k = sysread( $h->{m}, $d, 65536 );
    return '' unless $k;
    return $d;
}

# Read until MARKER is in the output or the deadline (ms, monotonic) passes: the time seen, or undef.
sub wait_for {
    my ( $h, $marker, $deadline ) = @_;
    for ( ; ; ) {
        my $left = ( $deadline - now() ) / 1000;
        return undef if $left <= 0;
        my $d = rd( $h, $left > 0.2 ? 0.2 : $left );
        next unless defined $d;
        return undef if $d eq '';
        $h->{buf} .= $d;
        if ( index( $h->{buf}, $marker ) >= 0 ) { $h->{buf} = ''; return now(); }
        $h->{buf} = substr( $h->{buf}, -64 ) if length( $h->{buf} ) > 4096;
    }
}

sub drain {
    my ( $h, $quiet, $cap ) = @_;
    my $end = now() + $cap * 1000;
    while ( now() < $end ) {
        my $d = rd( $h, $quiet );
        last if !defined $d || $d eq '';
    }
    $h->{buf} = '';
}

sub finish {
    my $h = shift;
    close $h->{m};
    kill 'HUP', $h->{pid};
    for ( 1 .. 20 ) { last if waitpid( $h->{pid}, POSIX::WNOHANG() ) != 0; select( undef, undef, undef, 0.1 ); }
    if ( kill 0, $h->{pid} ) { kill 'KILL', $h->{pid}; waitpid( $h->{pid}, 0 ); }
}

# One keystroke to echo; ms, or undef when no echo arrived within 2 s.
sub echo_once {
    my $h  = shift;
    my $t0 = now();
    syswrite( $h->{m}, '%' );
    my $t = wait_for( $h, '%', $t0 + 2000 );
    return defined $t ? $t - $t0 : undef;
}

my $mode = shift @ARGV // '';
if ( $mode eq 'now' ) { printf "%.3f\n", now(); exit 0; }

if ( $mode eq 'session' ) {
    my ( $sock, $file ) = @ARGV;
    open( my $sf, "<", $file ) or die "session: $file: $!\n";
    my $script = do { local $/; <$sf> };
    chomp $script;
    socket( my $s, PF_UNIX, SOCK_STREAM, 0 ) or die "socket: $!\n";
    connect( $s, pack_sockaddr_un($sock) ) or die "connect $sock: $!\n";
    my $bytes = sub { pack( 'V', length $_[0] ) . $_[0] };
    my $send  = sub { my ( $tag, $p ) = @_; syswrite( $s, pack( 'V C', length($p) + 1, $tag ) . $p ) or die "send: $!\n"; };
    my $recv  = sub {
        my $l = '';
        while ( length $l < 4 ) { sysread( $s, my $c, 4 - length $l ) or die "daemon closed\n"; $l .= $c; }
        my $n = unpack( 'V', $l );
        my $f = '';
        while ( length $f < $n ) { sysread( $s, my $c, $n - length $f ) or die "daemon closed\n"; $f .= $c; }
        return ( ord( substr( $f, 0, 1 ) ), substr( $f, 1 ) );
    };
    $send->( 1, pack( 'V', 3 ) . $bytes->('g4') );
    my ($t) = $recv->();
    die "no hello reply (tag $t)\n" unless $t == 1;
    my $o = pack( 'V', 3 ) . $bytes->('sh') . $bytes->('-c') . $bytes->($script) . $bytes->('/');
    $o .= pack( 'V', 1 ) . $bytes->('PATH') . $bytes->( $ENV{PATH} // '/usr/bin:/bin' );
    $o .= pack( 'V', 0 ) . "\0\0\0\0" . $bytes->( ( $ENV{XDG_RUNTIME_DIR} // '/' ) . '/w' );
    $send->( 2, $o );
    ( $t, my $p ) = $recv->();
    die "open refused (tag $t): $p\n" unless $t == 3;
    $| = 1;
    printf "pid %d\n", unpack( 'V', $p );
    1 while sysread( STDIN, my $x, 4096 );
    exit 0;
}

if ( $mode eq 'first' ) {
    my ( $marker, $to, @cmd ) = @ARGV;
    my $t0 = now();
    my $h  = spawn(@cmd);
    my $t  = wait_for( $h, $marker, $t0 + $to * 1000 );
    finish($h);
    die "FIRST timeout\n" unless defined $t;
    printf "FIRST ms=%.3f\n", $t - $t0;
    exit 0;
}

if ( $mode eq 'exit' ) {
    my ( $to, @cmd ) = @ARGV;
    my $t0  = now();
    my $h   = spawn(@cmd);
    my $end = $t0 + $to * 1000;
    for ( ; ; ) {
        die "EXIT timeout\n" if now() > $end;
        my $d = rd( $h, 0.2 );
        last if defined $d && $d eq '';
    }
    my $t = now();
    waitpid( $h->{pid}, 0 );
    printf "EXIT ms=%.3f code=%d\n", $t - $t0, $? >> 8;
    exit 0;
}

if ( $mode eq 'echo' ) {
    my ( $n, $ready, @cmd ) = @ARGV;
    my $h = spawn(@cmd);
    if ( $ready ne '-' ) { defined wait_for( $h, $ready, now() + 30000 ) or die "echo: no ready marker\n"; }
    drain( $h, 0.4, 3 );
    for ( 1 .. $n ) {
        my $ms = echo_once($h);
        if ( defined $ms ) { printf "ECHO ms=%.3f\n", $ms; } else { print "ECHO miss\n"; }
        drain( $h, 0.15, 1 );
    }
    finish($h);
    exit 0;
}

if ( $mode eq 'quiet' ) {    # QUIET MARKER SECS CMD...: bytes on the output over SECS after the first frame settles
    my ( $marker, $secs, @cmd ) = @ARGV;
    my $h = spawn(@cmd);
    defined wait_for( $h, $marker, now() + 30000 ) or die "quiet: no marker\n";
    drain( $h, 1.0, 5 );
    my ( $total, $end ) = ( 0, now() + $secs * 1000 );
    while ( now() < $end ) {
        my $d = rd( $h, 0.2 );
        next unless defined $d;
        last if $d eq '';
        $total += length $d;
    }
    finish($h);
    printf "QUIET bytes=%d secs=%s\n", $total, $secs;
    exit 0;
}

if ( $mode eq 'bytes' ) {
    my ( $ready, $secs, @cmd ) = @ARGV;
    my $h = spawn(@cmd);
    defined wait_for( $h, $ready, now() + 30000 ) or die "bytes: no ready marker\n";
    drain( $h, 1.0, 5 );
    my $t0 = now();
    syswrite( $h->{m}, "\n" );
    my @chunks;
    my $t1;
    my $buf = '';
    my $end = $t0 + ( $secs + 30 ) * 1000;
    while ( now() < $end ) {
        my $d = rd( $h, 0.2 );
        next unless defined $d;
        last if $d eq '';
        my $t = now();
        push @chunks, [ $t, length $d ];
        $buf .= $d;
        if ( index( $buf, 'G4DONE' ) >= 0 ) { $t1 = $t; last; }
        $buf = substr( $buf, -16 );
    }
    finish($h);
    die "bytes: workload did not finish\n" unless defined $t1;
    my ( $total, $inner ) = ( 0, 0 );
    for my $c (@chunks) {
        $total += $c->[1];
        $inner += $c->[1] if $c->[0] >= $t0 + 1000 && $c->[0] <= $t1 - 1000;
    }
    printf "BYTES total=%d secs=%.3f inner=%d inner_secs=%.3f\n", $total, ( $t1 - $t0 ) / 1000, $inner, ( $t1 - $t0 - 2000 ) / 1000;
    exit 0;
}

if ( $mode eq 'stall' ) {
    my ( $n, $secs, $ready, $floodend, @cmd ) = @ARGV;
    my $a = spawn(@cmd);
    my $b = spawn(@cmd);
    for my $h ( $a, $b ) { defined wait_for( $h, $ready, now() + 30000 ) or die "stall: no ready marker\n"; }
    drain( $_, 0.4, 3 ) for ( $a, $b );
    my ( @base, @during, $miss );
    for ( 1 .. $n ) {
        my $ms = echo_once($b);
        push @base, $ms if defined $ms;
        drain( $_, 0.15, 1 ) for ( $a, $b );
    }
    syswrite( $b->{m}, "\n" );    # starts the flood; viewer A is not read from here
    my $t0 = now();
    $b->{buf} = '';
    while ( now() < $t0 + $secs * 1000 ) {
        my $ms = echo_once($b);
        if ( defined $ms ) { push @during, $ms; } else { $miss++; }
        select( undef, undef, undef, 0.1 );
    }
    my $t1  = now();
    my $ok  = defined wait_for( $a, $floodend, $t1 + 60000 );
    my $rec = now() - $t1;
    finish($_) for ( $a, $b );
    printf "STALL base=%s during=%s miss=%d recover_ms=%.3f marker=%s\n", join( ',', map { sprintf '%.3f', $_ } @base ),
      join( ',', map { sprintf '%.3f', $_ } @during ), $miss // 0, $rec, $ok ? 'ok' : 'MISSING';
    exit 0;
}

die "g4pty: unknown mode '$mode'\n";
