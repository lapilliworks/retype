#!/usr/bin/perl
# Bound a native test command and clean up its process group.
use strict;
use warnings;
use Errno;
use IO::Select;
use POSIX qw(WNOHANG WIFEXITED WEXITSTATUS WIFSIGNALED WTERMSIG);
use Time::HiRes qw(CLOCK_MONOTONIC clock_gettime sleep);

my ($seconds, $grace, @command) = @ARGV;
if (!defined($seconds) || $seconds !~ /\A[1-9][0-9]*\z/
	|| !defined($grace) || $grace !~ /\A[1-9][0-9]*\z/ || !@command) {
	print STDERR "Usage: run-bounded.pl SECONDS GRACE COMMAND [ARG ...]\n";
	exit 125;
}

my $interrupted = 0;
$SIG{INT} = sub { $interrupted ||= 2 };
$SIG{TERM} = sub { $interrupted ||= 15 };
$SIG{HUP} = sub { $interrupted ||= 1 };
$SIG{PIPE} = 'IGNORE';

sub now {
	return clock_gettime(CLOCK_MONOTONIC);
}

sub event {
	my ($pipe, $message) = @_;
	my $length = length($message);
	my $written = syswrite($pipe, $message);
	POSIX::_exit(125) if !defined($written) || $written != $length;
}

sub execute {
	my (@args) = @_;
	$SIG{INT} = 'DEFAULT';
	$SIG{TERM} = 'DEFAULT';
	$SIG{HUP} = 'DEFAULT';
	$SIG{PIPE} = 'DEFAULT';
	{
		no warnings 'exec';
		exec { $args[0] } @args;
	}
	my $code = $!{ENOENT} ? 127 : 126;
	print STDERR "Cannot execute $args[0]: $!\n";
	POSIX::_exit($code);
}

sub supervise {
	my ($events, $control, $parent_group, @args) = @_;
	$SIG{INT} = 'IGNORE';
	$SIG{TERM} = 'IGNORE';
	$SIG{HUP} = 'IGNORE';
	if (!defined(POSIX::setpgid(0, 0))) {
		event($events, "ERROR Cannot create command process group: $!\n");
		POSIX::_exit(125);
	}
	# Do not start a command until the parent can safely signal its group.
	event($events, "READY\n");
	my $read = sysread($control, my $start, 1);
	POSIX::_exit(125) if !defined($read) || $read != 1 || $start ne 'S';

	my $child = fork();
	if (!defined($child)) {
		event($events, "ERROR Cannot start command: $!\n");
		POSIX::_exit(125);
	}
	if ($child == 0) {
		close($events);
		close($control);
		execute(@args);
	}
	my $waited;
	do { $waited = waitpid($child, 0) } while ($waited < 0 && $!{EINTR});
	if ($waited != $child) {
		event($events, "ERROR Cannot wait for command: $!\n");
		POSIX::_exit(125);
	}
	my $status = $?;
	# Keep this PID reserved while the parent checks the old group for leaks.
	if (!defined(POSIX::setpgid(0, $parent_group))) {
		event($events, "ERROR Cannot release command process group: $!\n");
		POSIX::_exit(125);
	}
	event($events, "EXIT $status\n");
	sysread($control, my $acknowledgement, 1);
	POSIX::_exit(0);
}

sub exit_code {
	my ($status) = @_;
	return WEXITSTATUS($status) if WIFEXITED($status);
	return 128 + WTERMSIG($status) if WIFSIGNALED($status);
	return 125;
}

sub group_exists {
	my ($leader) = @_;
	return 1 if kill(0, -$leader);
	return 0 if $!{ESRCH};
	print STDERR "Cannot inspect command process group $leader: $!\n";
	return 1;
}

sub signal_group {
	my ($signal, $leader) = @_;
	return if kill($signal, -$leader);
	print STDERR "Cannot send $signal to command process group $leader: $!\n" unless $!{ESRCH};
}

sub pause_until {
	my ($deadline) = @_;
	while (now() < $deadline) {
		my $remaining = $deadline - now();
		sleep($remaining < 0.05 ? $remaining : 0.05) if $remaining > 0;
	}
}

sub stop_group {
	my ($leader, $wait) = @_;
	signal_group('TERM', $leader);
	pause_until(now() + $wait);
	signal_group('KILL', $leader);
}

sub reap {
	my ($leader) = @_;
	my $deadline = now() + 2;
	while (now() < $deadline) {
		my $waited = waitpid($leader, WNOHANG);
		return 1 if $waited == $leader;
		if ($waited < 0 && !$!{EINTR}) {
			print STDERR "Cannot reap command supervisor $leader: $!\n";
			return 0;
		}
		sleep(0.05);
	}
	print STDERR "Command supervisor $leader did not stop.\n";
	return 0;
}

sub process_snapshot {
	my ($leader) = @_;
	print STDERR "Command process snapshot for group $leader (PID PPID PGID STAT WCHAN COMMAND):\n";
	# Bound ps with the same supervisor, without recursively inspecting ps itself.
	local $ENV{_RETYPE_BOUNDED_INSPECTION} = 1;
	my $pid = open(my $pipe, '-|', $^X, __FILE__, '2', '1',
		'ps', '-ww', '-axo', 'pid=,ppid=,pgid=,stat=,wchan=,command=');
	if (!$pid) {
		print STDERR "Cannot start process snapshot: $!\n";
		return;
	}
	my $selection = IO::Select->new($pipe);
	my $deadline = now() + 5;
	my ($buffer, $size, $matched, $forwarded) = ('', 0, 0, 0);
	while (1) {
		if ($interrupted && !$forwarded) {
			kill('TERM', $pid);
			$forwarded = 1;
		}
		my $remaining = $deadline - now();
		if ($remaining <= 0) {
			print STDERR "Process snapshot helper did not finish in time.\n";
			kill('KILL', $pid);
			last;
		}
		my @ready = $selection->can_read($remaining < 0.1 ? $remaining : 0.1);
		next if !@ready;
		my $read = sysread($pipe, my $chunk, 4096);
		if (!defined($read)) {
			next if $!{EINTR};
			print STDERR "Cannot read process snapshot: $!\n";
			last;
		}
		last if $read == 0;
		$size += $read;
		if ($size > 512 * 1024) {
			print STDERR "Process snapshot exceeded its output limit.\n";
			last;
		}
		$buffer .= $chunk;
		while ($buffer =~ s/\A([^\n]*)\n//) {
			my $line = $1;
			next unless $line =~ /^\s*\d+\s+\d+\s+(\d+)\s+\S+\s+\S+\s+/ && $1 == $leader;
			print STDERR "$line\n" if $matched < 64;
			$matched++;
		}
	}
	my $closed = close($pipe);
	my $status = $?;
	print STDERR "Process snapshot failed or timed out (exit ", exit_code($status), ").\n" unless $closed;
	print STDERR "No processes from the command group were recorded.\n" unless $matched;
	print STDERR "Additional command-group processes were omitted.\n" if $matched > 64;
}

sub monitor {
	my ($reader, $control, $deadline) = @_;
	my $selection = IO::Select->new($reader);
	my $buffer = '';
	my $result = { ready => 0, completed => 0, code => 125 };
	while (1) {
		if ($interrupted) {
			$result->{code} = 128 + $interrupted;
			last;
		}
		my $remaining = $deadline - now();
		if ($remaining <= 0) {
			print STDERR "Command exceeded ${seconds}s; stopping its process group.\n";
			$result->{code} = 124;
			last;
		}
		my @ready = $selection->can_read($remaining < 0.1 ? $remaining : 0.1);
		next if !@ready;
		my $read = sysread($reader, my $chunk, 4096);
		if (!defined($read)) {
			next if $!{EINTR};
			print STDERR "Cannot read command supervisor: $!\n";
			last;
		}
		if ($read == 0) {
			print STDERR "Command supervisor stopped without a result.\n";
			last;
		}
		$buffer .= $chunk;
		while ($buffer =~ s/\A([^\n]*)\n//) {
			my $message = $1;
			if ($message eq 'READY' && !$result->{ready}) {
				$result->{ready} = 1;
				if (!syswrite($control, 'S')) {
					print STDERR "Cannot start command supervisor: $!\n";
					return $result;
				}
			} elsif ($message =~ /\AEXIT ([0-9]+)\z/ && $result->{ready}) {
				$result->{completed} = 1;
				$result->{code} = exit_code($1);
				return $result;
			} else {
				print STDERR "Command supervisor failed: $message\n";
				return $result;
			}
		}
	}
	return $result;
}

pipe(my $event_read, my $event_write) or die "Cannot create command event pipe: $!\n";
pipe(my $control_read, my $control_write) or die "Cannot create command control pipe: $!\n";
my $parent_group = getpgrp();
my $leader = fork();
defined($leader) or die "Cannot start command supervisor: $!\n";
if ($leader == 0) {
	close($event_read);
	close($control_write);
	supervise($event_write, $control_read, $parent_group, @command);
}
close($event_write);
close($control_read);
my $result = monitor($event_read, $control_write, now() + $seconds);
if ($result->{completed}) {
	if (group_exists($leader)) {
		print STDERR "Command left subprocesses running (exit $result->{code}); stopping process group $leader.\n";
		stop_group($leader, 1);
		$result->{code} = 125;
	}
	if (!syswrite($control_write, 'Q')) {
		print STDERR "Cannot release command supervisor: $!\n";
		$result->{code} = 125;
	}
} else {
	if ($result->{ready}) {
		if ($result->{code} == 124 && !$ENV{_RETYPE_BOUNDED_INSPECTION}) {
			signal_group('TERM', $leader);
			process_snapshot($leader);
		}
		stop_group($leader, $grace);
	} else {
		kill('TERM', $leader);
		pause_until(now() + $grace);
	}
	kill('KILL', $leader);
}
close($event_read);
close($control_write);
$result->{code} = 125 if !reap($leader) && $result->{code} == 0;
$result->{code} = 128 + $interrupted if $interrupted && $result->{code} != 124;
exit $result->{code};
