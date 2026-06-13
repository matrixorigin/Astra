import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { Search, Star } from 'lucide-react';
import { Avatar } from '@/components/ui/avatar';
import { Button } from '@/components/ui/button';
import { Card } from '@/components/ui/card';
import { EmptyState } from '@/components/ui/empty-state';
import { IconButton } from '@/components/ui/icon-button';
import { Input } from '@/components/ui/input';
import { ListItem } from '@/components/ui/list-item';
import { Menu, MenuItem } from '@/components/ui/menu';
import { Modal } from '@/components/ui/modal';
import { PageHeader } from '@/components/ui/page-header';
import { Popover } from '@/components/ui/popover';
import { SearchField } from '@/components/ui/search-field';
import { SidebarSection } from '@/components/ui/sidebar-section';
import { Textarea } from '@/components/ui/textarea';

describe('ui primitives', () => {
  it('renders Button as button and link variants', () => {
    render(
      <>
        <Button>Save</Button>
        <Button href="/projects">Projects</Button>
      </>,
    );
    expect(screen.getByRole('button', { name: 'Save' })).toBeInTheDocument();
    expect(screen.getByRole('link', { name: 'Projects' })).toHaveAttribute('href', '/projects');
  });

  it('renders Input and Textarea controls', () => {
    render(
      <>
        <Input aria-label="Name" />
        <Textarea aria-label="Instructions" />
      </>,
    );
    expect(screen.getByLabelText('Name')).toBeInTheDocument();
    expect(screen.getByLabelText('Instructions')).toBeInTheDocument();
  });

  it('renders Card, ListItem, Avatar, EmptyState, PageHeader, and SearchField', () => {
    render(
      <>
        <Card interactive href="/x">Card body</Card>
        <ListItem title="List title" subtitle="List subtitle" icon={Search} />
        <Avatar name="Astra User" />
        <EmptyState icon={Star} title="Nothing here" description="Empty" />
        <PageHeader title="Header" action={<Button>Action</Button>} />
        <SearchField aria-label="Search field" />
      </>,
    );
    expect(screen.getByRole('link', { name: 'Card body' })).toHaveAttribute('href', '/x');
    expect(screen.getByText('List title')).toBeInTheDocument();
    expect(screen.getByText('AU')).toBeInTheDocument();
    expect(screen.getByText('Nothing here')).toBeInTheDocument();
    expect(screen.getByText('Header')).toBeInTheDocument();
    expect(screen.getByLabelText('Search field')).toBeInTheDocument();
  });

  it('renders IconButton with accessible label', () => {
    render(<IconButton icon={Search} label="Search now" />);
    expect(screen.getByRole('button', { name: 'Search now' })).toBeInTheDocument();
  });

  it('renders Modal content when open', () => {
    render(
      <Modal open onOpenChange={vi.fn()} title="Dialog title">
        <p>Dialog body</p>
      </Modal>,
    );
    expect(screen.getByText('Dialog title')).toBeInTheDocument();
    expect(screen.getByText('Dialog body')).toBeInTheDocument();
  });

  it('opens Popover and Menu content from their triggers', async () => {
    const user = userEvent.setup();
    render(
      <>
        <Popover trigger={<button type="button">Open popover</button>}>
          <p>Popover body</p>
        </Popover>
        <Menu trigger={<button type="button">Open menu</button>}>
          <MenuItem>Menu body</MenuItem>
        </Menu>
      </>,
    );
    await user.click(screen.getByRole('button', { name: 'Open popover' }));
    expect(screen.getByText('Popover body')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Open menu' }));
    expect(screen.getByText('Menu body')).toBeInTheDocument();
  });

  it('toggles SidebarSection content', async () => {
    const user = userEvent.setup();
    render(
      <SidebarSection label="Recents">
        <p>Recent item</p>
      </SidebarSection>,
    );
    expect(screen.getByText('Recent item')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Recents' }));
    expect(screen.queryByText('Recent item')).not.toBeInTheDocument();
  });
});
