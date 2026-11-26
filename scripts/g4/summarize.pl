# G4 summary: reads RUNDIR/*.raw (lines `leg=L run=N kind=K warm=0|1 KEY k=v ...`) and prints the results table.
# Warm-up runs (warm=1) are listed in the raw files and excluded here. perl-base only.
use strict;
use warnings;
my $dir = shift // die "usage: summarize.pl RUNDIR\n";

sub med { my @s = sort { $a <=> $b } @_; return '-' unless @s; my $n = @s; return $n % 2 ? $s[ $n / 2 ] : ( $s[ $n / 2 - 1 ] + $s[ $n / 2 ] ) / 2; }
sub p95 { my @s = sort { $a <=> $b } @_; return '-' unless @s; return $s[ int( 0.95 * ( @s - 1 ) + 0.5 ) ]; }
sub f   { return $_[0] eq '-' ? '-' : sprintf( '%.1f', $_[0] ); }

my %d;    # leg -> list of {kind, run, kv}
for my $file ( glob "$dir/*.raw" ) {
    open( my $fh, '<', $file ) or next;
    while (<$fh>) {
        chomp;
        my %kv = /(\w+)=(\S+)/g;
        next if ( $kv{warm} // 0 ) == 1 || !defined $kv{leg};
        push @{ $d{ $kv{leg} } }, \%kv;
    }
}

print "G4 results ($dir)\n";
if ( $d{first} ) {
    print "\nfirst-frame (ms; one value per attach)\n";
    printf "  %-10s %4s %9s %9s %9s %9s\n", 'kind', 'n', 'median', 'p95', 'min', 'max';
    my %by;
    push @{ $by{ $_->{kind} } }, $_->{ms} for grep { defined $_->{ms} } @{ $d{first} };
    my %m;
    for my $k ( sort keys %by ) {
        my @v = @{ $by{$k} };
        $m{$k} = med(@v);
        printf "  %-10s %4d %9s %9s %9s %9s\n", $k, scalar @v, f( med(@v) ), f( p95(@v) ), f( ( sort { $a <=> $b } @v )[0] ), f( ( sort { $a <=> $b } @v )[-1] );
    }
    printf "  cterm - handshake (median): %s ms   (target: at most 50)\n", f( $m{cterm} - $m{handshake} ) if defined $m{cterm} && defined $m{handshake};
    printf "  cterm - noise (median): %s ms\n", f( $m{cterm} - $m{noise} ) if defined $m{cterm} && defined $m{noise};
}
if ( $d{echo} ) {
    print "\necho (ms; all samples pooled over runs; link RTT beside it in two-host mode)\n";
    printf "  %-10s %4s %5s %9s %9s %9s\n", 'kind', 'n', 'miss', 'median', 'p95', 'max';
    my ( %by, %miss, %m );
    for ( @{ $d{echo} } ) { if ( defined $_->{ms} ) { push @{ $by{ $_->{kind} } }, $_->{ms} } else { $miss{ $_->{kind} }++ } }
    for my $k ( sort keys %by ) {
        my @v = @{ $by{$k} };
        $m{$k} = med(@v);
        printf "  %-10s %4d %5d %9s %9s %9s\n", $k, scalar @v, $miss{$k} // 0, f( med(@v) ), f( p95(@v) ), f( ( sort { $a <=> $b } @v )[-1] );
    }
    if ( open( my $r, '<', "$dir/rtt.txt" ) ) { my $l = <$r>; chomp $l; print "  link RTT: $l\n"; }
    printf "  cterm - noise (median): %s ms   (target: at most 5 beyond link RTT)\n", f( $m{cterm} - $m{noise} ) if defined $m{cterm} && defined $m{noise};
}
if ( $d{bytes} ) {
    print "\nbytes (viewer output vs the bytes the workload wrote; per run, medians)\n";
    printf "  %-8s %4s %12s %12s %8s %14s\n", 'workload', 'n', 'viewer B', 'raw B', 'ratio', 'inner B/s';
    my %by;
    push @{ $by{ $_->{kind} } }, $_ for @{ $d{bytes} };
    for my $k ( sort keys %by ) {
        my @r = @{ $by{$k} };
        my @tot = map { $_->{total} // $_->{bytes} } @r;
        my @raw = map { $_->{raw} // 0 } @r;
        my @rat = map { $_->{raw} ? ( $_->{total} / $_->{raw} ) : () } @r;
        my @bps = map { $_->{inner_secs} ? ( ( $_->{inner} // $_->{bytes} ) / $_->{inner_secs} ) : () } @r;
        printf "  %-8s %4d %12s %12s %8s %14s\n", $k, scalar @r, f( med(@tot) ), f( med(@raw) ), @rat ? sprintf( '%.2f', med(@rat) ) : '-', f( med(@bps) );
    }
    print "  targets: idle sends no bytes; build log at most 2.00 x raw\n";
}
if ( $d{stall} ) {
    print "\nstall (viewer A unread for the stall window; viewer B echo, ms)\n";
    printf "  %4s %6s %10s %10s %8s %6s %11s %s\n", 'run', 'n base', 'base med', 'during med', 'rise', 'miss', 'recover ms', 'A marker';
    for ( @{ $d{stall} } ) {
        my @b = split /,/, $_->{base}   // '';
        my @s = split /,/, $_->{during} // '';
        my ( $bm, $sm ) = ( med(@b), med(@s) );
        printf "  %4s %6d %10s %10s %8s %6s %11s %s\n", $_->{run}, scalar @b, f($bm), f($sm), ( $bm eq '-' || $sm eq '-' ) ? '-' : f( $sm - $bm ), $_->{miss}, f( $_->{recover_ms} ), $_->{marker};
    }
    print "  target: rise at most 10 ms; the stalled viewer's marker must arrive after it reads again\n";
}
