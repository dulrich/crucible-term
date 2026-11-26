# G4 workload run inside the session: KIND SECONDS COUNTFILE. Writes its own byte count to
# COUNTFILE (`bytes N`) before it prints G4DONE. KIND: idle (writes nothing), build (a scrolling
# build log, about 2000 lines/s), redraw (a full-screen 80x24 repaint, 20 frames/s).
use strict;
use warnings;
my ( $kind, $secs, $count ) = @ARGV;
$| = 1;
my $n   = 0;
my $out = sub { $n += length $_[0]; syswrite( STDOUT, $_[0] ); };
my $end = time() + $secs;
my $i   = 0;
while ( time() < $end ) {
    if ( $kind eq 'build' ) {
        for ( 1 .. 20 ) { $i++; $out->( sprintf "   Compiling crate-%d v0.%d.%d (/work/src/crate-%d/src/mod%d.rs)\r\n", $i % 97, $i % 7, $i % 13, $i % 97, $i ); }
        select( undef, undef, undef, 0.01 );
    }
    elsif ( $kind eq 'redraw' ) {
        $i++;
        my $f = "\e[H";
        $f .= sprintf( "row %02d frame %06d %s\e[K\r\n", $_, $i, 'abcdefghij' x 5 ) for 1 .. 23;
        $out->($f);
        select( undef, undef, undef, 0.05 );
    }
    else { select( undef, undef, undef, 0.2 ); }
}
open( my $c, '>', $count ) or die "workload: $count: $!\n";
print $c "bytes $n\n";
close $c;
print "G4DONE\r\n";
